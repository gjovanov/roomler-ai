# Vault: IAM for secrets

The vault keeps an organization's secrets: database passwords, API tokens,
kubeconfigs, signing keys. Every value is encrypted at rest under a key of its
own, named by an RRN, and every decision about it is recorded. It is built for
Hive's agent sessions, which use a secret without the model ever reading it.
People use it too.

> Spec, steps and the field log: [`fr/FR-90-hive-agent-sessions.md`](fr/FR-90-hive-agent-sessions.md)
> §3e (#1827). The whole design (roles, Cedar policies, leases, the modes a
> session gets a secret in) is [`roomler-hive-design.md`](roomler-hive-design.md)
> §8. **This page describes what is built: P3-1, the store; P3-2a, roles and
> Cedar policies (§7); P3-2b, a session's role, decided at its start and at
> every move (§8); P3-3a, the key a device's daemon holds for the values it is
> leased (§9); P3-3b, the vault's half of a lease (§10); P3-3c, how the
> vault takes a lease back (§11); P3-4a, the daemon's lease client (§12);
> P3-4b, what a session may know of the vault: its metadata tools, decided
> by Cedar's own `secret:list` (§13); P3-4c-1, `vault_exec`, the first
> tool that uses a value: one command run with values in its environment
> (§14); P3-4c-2, `vault_render`: a template written with values into the
> session's secrets directory, for as long as its leases (§15); P3-4c-3,
> `vault_request`: a lease asked for ahead, which a use takes (§16);
> P3-4d, what a device reports doing with its leases where its owner has it
> report, kept apart from the decisions (§17); P3-4e, the approval a policy
> may ask for: a card in the session's room that one of its drivers
> answers, a session's tools that wait for the answer, and the card's
> buttons in the web app (§18); P3-5a, a secret's `audiences` held to for
> `proxy` mode, the first step of the proxies (§7); P3-5b-1, the Postgres
> engine a proxy endpoint serves; P3-5b-2, `vault_connect` and its
> endpoint, which signs in to a database as a secret's user for the
> session's account; and P3-5b-3, the exchange bound to the certificate
> the proxy verified (§19).** No released
> agent asks for a lease yet: no release build carries the `vault` feature.

## 1. Custody: who can read a secret

> [!WARNING]
> **Managed custody.** The vault module holds the key-encryption key (KEK)
> and can decrypt every secret it stores. Whoever controls the server, or
> holds both the KEK and a copy of the database, can read them all. That is
> the v1 model.

What that means in practice:

| Who | Can read values? | Why |
|---|---|---|
| the server process | yes | it holds the KEK ([`KekState`](../crates/modules/vault/src/crypto.rs#L242)) |
| the cluster's operator | yes | the KEK is a secret on the cluster (§3) |
| someone with a database dump only | no | every value is sealed; the data keys are wrapped to the KEK |
| someone with a database dump **and** the KEK | yes | that is all it takes, so keep the KEK out of every backup the database goes into |
| a member of the organization, an administrator included | no | no route answers a value in P3-1 (§6) |
| the daemon running a session | from P3-4, what a lease releases | the vault answers a lease (§10) with the value sealed to the key that daemon holds (§9); no released agent asks for one yet |
| the session's own process, or its model | no, except what a materializing mode hands it | a leased value is opened in the daemon, never in the harness's process, which runs as another account. `exec` and `file` hand a value on: to one command run as the session's account (§14), or in a file that account reads (§15). That is why each takes a permit, passes the guardrails, and waits for a person's approval |

Customer-managed keys (the KEK in the organization's own KMS, revocable by it)
and sovereign custody (only the organization's devices can unwrap) are P7.

## 2. Envelope encryption

Every value written gets a fresh 32-byte data key (DEK). The value is sealed
under it with XChaCha20-Poly1305, and the DEK is wrapped to the KEK with
[`age`](https://age-encryption.org). The KEK is an `age` X25519 identity, so a
wrapped DEK is an ordinary `age` file and `age-keygen` makes the KEK.

```mermaid
flowchart LR
    V["value<br/>(PUT …/value)"] --> S["XChaCha20-Poly1305<br/>24-byte random nonce"]
    D["DEK<br/>32 random bytes, one per version"] --> S
    B["binding: org · secret · field · version<br/>(associated data)"] --> S
    S --> C[("vault_versions.ciphertext")]
    D --> W["age encrypt"]
    K["KEK recipient<br/>age1…"] --> W
    W --> DW[("vault_versions.dek_wrapped")]
    K -.->|"recorded as"| KR[("vault_versions.kek")]
```

| Stored with a version | What it is |
|---|---|
| `ciphertext` | the value under the DEK |
| `nonce` | 24 random bytes, one per version |
| `dek_wrapped` | the DEK, `age`-encrypted to the KEK |
| `kek` | the KEK's recipient (`age1…`): public, kept so a rotation can tell which key wraps which version |

> [!IMPORTANT]
> **A seal is bound to its place.** The associated data names the
> organization, the secret, the field and the version number
> ([`Binding`](../crates/modules/vault/src/crypto.rs#L46)). Whoever can write
> the database can copy a ciphertext from one row into another. With the
> binding, the copied value fails to open, instead of answering for the
> secret it was moved into. The KEK is not in the binding: a rotation re-wraps
> the DEKs and leaves every value alone.

The vault brings its own keys. It derives nothing from the JWT secret or the
DERP ticket key.

**Recovery without the server.** `age -d -i kek.txt` decrypts a version's
`dek_wrapped` to its DEK, and the DEK opens the `ciphertext` with the binding
as associated data: `roomler-vault/v1`, the organization id, the secret id,
the field and the version number, NUL-separated
([`a_wrapped_data_key_is_an_ordinary_age_file`](../crates/modules/vault/src/crypto.rs#L478)
is that recipe as a test).

## 3. The KEK

| Setting | Env | Holds |
|---|---|---|
| `vault.kek` | `ROOMLER__VAULT__KEK` | the identity, `AGE-SECRET-KEY-1…`, or an `age-keygen` file's whole text |
| `vault.kek_file` | `ROOMLER__VAULT__KEK_FILE` | the path of an `age-keygen` file |

```sh
age-keygen -o kek.txt          # keep it out of every backup the database is in
```

- A self-hoster keeps the KEK in a file. Hosted roomler.ai keeps it in a sealed
  secret on the cluster, which has no cloud KMS under it.
- Exactly one identity. Blank lines and `#` comments are skipped, as
  `age-keygen` writes them. More than one identity is refused: a KEK rotation
  is not built yet.
- **Both settings set is refused**, not guessed: the two may disagree.
- **Never a boot refusal.** With no KEK, or one that does not parse, the vault
  still mounts. Its metadata routes work, and the two routes that need the
  key (`PUT …/value`, `POST …/verify`) answer **503** with the reason. The
  reason is always a fixed sentence and never repeats the setting.
- The KEK never prints. `VaultSettings`' `Debug` is written by hand
  ([`settings.rs`](../crates/config/src/settings.rs#L102)), and so is the
  KEK's ([`crypto.rs`](../crates/modules/vault/src/crypto.rs#L121)). The boot
  log names the recipient only.
- `GET …/vault` reports `other_kek`: how many versions were wrapped to a KEK
  other than the one loaded. Above zero after a key change, that count is
  every value the old key held, now unreadable. `POST …/verify` names them.

## 4. Secrets, versions, RRNs

A **secret** is a name and its rules, and never a value:

| Field | Meaning |
|---|---|
| `path` | unique among the organization's live secrets; lowercase segments of `a-z 0-9 . _ -` joined by `/`. Never changes |
| `kind` | `static` (one field, `value`) or `structured` (named fields: `username`, `password`…). `dynamic`, `template` and `llm_credential` come with the steps that give them meaning |
| `audiences` | where a value may be presented: host patterns, a knowhow entity, a named upstream. Stored in P3-1; the daemon holds a use to them from P3-5 |
| `exec_allow` | the programs a value may be handed to in `exec` mode, each by its **bare** name (`psql`, never a path: a path is refused on write, since a device asks by name and resolves it itself). `vault_exec` runs nothing else (§14) |
| `tags`, `owner_id`, `rotation_days`, `description` | metadata; tags get one spelling, because a policy matches them by equality |
| `disabled` | P3-5's per-secret kill switch: a disabled secret is never used. An administrator still reads its metadata and writes its value |

Its **RRN** is `rrn:{org slug}:secret:{path}`
([`rrn.rs`](../crates/modules/vault/src/rrn.rs)). The organization's slug is
part of the name, so a reference to another organization's secret cannot be
written down. The RRN is rendered when shown, and never stored, so the name and
the row cannot disagree.

> [!NOTE]
> **One spelling per name.** Paths, fields and tags are lowercase by rule, not
> folded. Two paths that differ only in case would be one name to a person and
> two to an index. A field name is a key in the stored document
> (`fields.<name>`), so `.` and `$` are refused: either would make it a path or
> an operator to the database.

A **version** is one value written to one field. Each field counts on its own:

- **The number comes from an `$inc` on the secret**, so two writers never get
  one number. A write that fails after it leaves a gap; a number is never
  reused.
- **`current` moves by `$max`** once the row is stored, so a slower writer of an
  older number never moves it back.
- **A field keeps its last 10 versions.** Older ones are destroyed: an old value
  is no less sensitive for being old.
- `expires_at`, when given, is honoured by the first step that uses a value:
  P3-3's leases never hand out an expired version.

**Deleting a secret** tombstones it with a compare-and-set, so of two deletes
one acts, and then destroys every version. There is no undo. The path is free
again, and a new secret there gets a new id. The tombstone comes first: a
version delete that fails after it leaves ciphertext no route reaches, a leak
to sweep, never a secret brought back.

## 5. The org gate and the switch

| Switch | Effect |
|---|---|
| `[modules] vault` (`ROOMLER__MODULES__VAULT`) | **default OFF**, like `hive`: no route is mounted |
| `vault.tenants` (`ROOMLER__VAULT__TENANTS=<id>,<id>`) | the organizations the vault serves. A list alone mounts the module, the form a hosted server uses, because no older image reads it. Clearing it is the kill switch |

`vault.tenants` reads like `hive.tenants`: empty or `*` is every organization,
and an entry that is not an organization id matches nothing
([`scope.rs`](../crates/modules/vault/src/scope.rs)). A member of an
organization the vault does not serve is told `404 the vault is not available
to this organization` on every route. Someone outside the organization is
told `403 not_a_member` first, and so learns nothing about whether it is
served.

## 6. Routes

All under `/api/tenant/{tenant_id}/vault`, for the organization's
`ADMINISTRATOR`s only ([`routes.rs`](../crates/modules/vault/src/routes.rs)).

| Method | Path | Does | Needs the KEK | Recorded as |
|---|---|---|---|---|
| GET | `` | the KEK's state (`ready` with its recipient, `missing`, `invalid` with the reason), the counts, `other_kek` | no | — |
| GET | `/secret` | the live secrets, by path; `?prefix=staging/` lists a folder | no | `secret:list` · `list` |
| POST | `/secret` | declare one: path, kind, rules. ⚠️ `value` here is an unknown field, refused **422**, so a value has one way in | no | `secret:write` · `create` |
| GET | `/secret/{id}` | one secret and its versions' metadata: number, KEK, author, expiry. The read never loads the sealed bytes | no | `secret:list` · `describe` |
| PATCH | `/secret/{id}` | change its rules. `path` and `kind` are unknown fields (**422**): a policy names a secret by its RRN, and a rename would quietly re-point every policy that names it | no | `secret:write` · `update` |
| DELETE | `/secret/{id}` | tombstone it and destroy every version | no | `secret:delete` · `delete` |
| PUT | `/secret/{id}/value` | write a field's next version, `{field?, value, expires_at?}`; answers the number | **yes** | `secret:write` · `value`, with the number |
| POST | `/verify` | open every version the organization holds (up to 10,000) and answer which do not open: `other_kek`, `dek_unreadable`, `tampered`. Never a value | **yes** | `vault:verify` · `verify` |
| GET | `/audit` | the decisions, newest first (`?before=`, `?limit=` up to 200) | no | — |
| GET | `/schema` | P3-2a — the Cedar schema policies are written against, and the actions, modes and OSes it names | no | — |
| GET · POST | `/role` | P3-2a — the vault roles; declare one `{name, description?}` | no | `policy:write` · `role_create` |
| DELETE | `/role/{name}` | P3-2a — tombstone a role; **409** while a live policy names it | no | `policy:write` · `role_delete` |
| GET · POST | `/policy` | P3-2a — the guardrails, then the organization's policies; add one `{name, description?, text}` | no | `policy:write` · `policy_create` |
| GET · PUT · DELETE | `/policy/{id}` | P3-2a — one policy; replace it `{…, rev}` from the revision read (**409** if it moved); tombstone it. A `guardrail:*` id is **400** for both writes | no | `policy:write` · `policy_update` / `policy_delete` |
| POST | `/simulate` | P3-2a — decide a request as the vault would, and say which policies decided it; `with` / `without` for a change not yet made | no | — (decides nothing for real) |
| POST | `/simulate/replay` | P3-2b — `{with?, without?, days?}`: the last 1 to 7 days of recorded Cedar decisions decided again under the change; `{replayed, unreadable, flipped, flips[…], truncated}` | no | — (decides nothing for real) |

> [!WARNING]
> **No route answers a value.** In P3-1 a value goes in through `PUT …/value`
> and comes out nowhere. A session's use is P3-3's lease. A person's
> `secret:read`, which puts the value in front of a human, is a later step with
> its own gate.

## 7. Roles and policies (P3-2a)

A session will assume one **role** at its start (P3-2b), and the
organization's **policies**, written in [Cedar](https://www.cedarpolicy.com),
say what each role may do. The mapping to AWS IAM is design §8.1; what is
built is the language, the checks, the guardrails and `simulate`
([`policy.rs`](../crates/modules/vault/src/policy.rs)).

```mermaid
flowchart LR
    W["an administrator writes a policy"] --> C{"checked before it is kept"}
    C -->|"one Cedar statement · valid against the schema, strictly ·<br/>names only this organization"| S[("vault_policies")]
    C -->|"anything else"| R["400, in words"]
    G["the guardrails<br/>(code, never rows)"] --> SET["the organization's set"]
    S --> SET
    SET --> D{"decide"}
    Q["a request: role:assume, secret:use or secret:list"] --> D
    D -->|"a permit applies, and no forbid"| A["allow"]
    D -->|"a forbid applies, or nothing permits"| N["deny"]
```

**The schema** ([`policy.cedarschema`](../crates/modules/vault/src/policy.cedarschema)),
namespace `Roomler`:

| Entity | What it is |
|---|---|
| `User` `in Group` | a person; one `Group` per roomler role they hold (`owner`, `admin`, `member`, … and the organization's own) |
| `Role` | a vault role, `Roomler::Role::"dev"` |
| `Session` `in Role` | an agent session in the role it assumed; `owner`, and `playbook` when it runs one |
| `Secret` `in SecretFolder` | `rrn:{org}:secret:{path}`, with `path`, `kind`, `tags`, (P3-4c-1) `exec_allow` and (P3-5a) `audiences`; a folder per path prefix, each in the one above |

| Action | Principal → resource | Context |
|---|---|---|
| `role:assume` | `User` → `Role` | `device {os, tags, owner}` |
| `secret:use` | `Session` → `Secret` | `mode`, `audience`, `ttl_s`, `device {os, tags, owner}`, `approval? {approver}` |
| `secret:list` (P3-4b, §13) | `Session` → `Secret` | `device {os, tags, owner}` |

`secret:list` is an action of its own, not a member of `secret:use`: an
action group would make "use implies list" hold only for policies written
with `in`, never for `==`. So a role lists nothing until a permit says it may,
and a listing permit uses nothing.

From P3-4e (§18) the approver is read in a `Group` per roomler role they
hold, as a person assuming a role is: `context.approval.approver in
Roomler::Group::"admin"` holds for an admin's yes.

> [!NOTE]
> Only the approver is read in their groups. A session's `owner` and a
> device's `owner` are named without them, so `principal.owner in
> Roomler::Group::…` never holds.

`os` is `linux`, `macos` or `windows`, exactly as the device rows store it; any
other spelling is refused, never lowercased into a match. A device's tags are
lowercased, because they are free-form and a policy compares by equality.

**The guardrails** are in every organization's set from its first decision.
They are code, not rows, so no organization can edit or lose one, and their
text names no organization:

| Guardrail | Forbids |
|---|---|
| `guardrail:windows-proxy-only` | any `secret:use` on Windows in a mode other than `proxy`: there is no Claude Code sandbox there (AC13; decision 17) |
| `guardrail:prod-never-materializes` | `secret:use` in `file`, `env` or `exec` for a secret tagged `prod` or under `prod/` |
| `guardrail:exec-only-allowed-programs` (P3-4c-1, §14) | `secret:use` in `exec` mode whose audience, the program a device hands the value to, is not on the secret's `exec_allow` |
| `guardrail:proxy-only-named-audiences` (P3-5a) | `secret:use` in `proxy` mode whose audience, the upstream the daemon authenticates to with the value, is not on the secret's `audiences`, exactly as written |

No guardrail stops a listing: all four are about using a value.

> [!WARNING]
> **Before P3-5a a secret's `audiences` were kept and shown, and enforced nowhere.** No device asked
> for a `proxy` lease before then, but one would have been released toward any audience. A
> `proxy` lease now reaches only an upstream its secret names, and a recorded request from before
> the field reads back naming none, so a replay refuses it as today's vault would. Matching is exact:
> `db.internal:5432` is not `db.internal`, `DB.internal:5432` or `db.internal:6432`.

**A policy is checked before it is kept** ([`check_policy`](../crates/modules/vault/src/policy.rs)):
exactly one static Cedar statement, valid against the schema in strict mode,
and naming only its organization:

- a `Secret` or `SecretFolder` literal must carry the organization's RRN
  prefix, with a valid path after it;
- a `Role` must be one of its vault roles;
- a `Group` must be one of its roomler roles;
- a `User` must be one of its members;
- a `Session` is never named: sessions come and go, and a policy for one would
  outlive it.

> [!IMPORTANT]
> - **Default deny; a `forbid` beats every `permit`.**
> - **A set that does not build is a deny, never an allow.** A stored policy
>   that no longer parses fails the whole set, rather than being skipped: the
>   one skipped could be a `forbid`.
> - **A role a live policy names cannot be deleted.** A role re-created under
>   the old name would inherit every policy that still named it. Whether a
>   policy names a role is read from the parsed policy, so no spacing or escape
>   in the text hides it.

**`simulate`** decides a request as the vault would and answers which policies
decided it: the permits that allowed it, the forbid or guardrail that denied
it, or nothing (the default deny). With `with` (policies as if added, each
checked as a write) and `without` (stored policies as if deleted, never a
guardrail), it answers for a change before it is made. It decides nothing for
real and records nothing. **`simulate/replay`** (P3-2b) takes the same
`with` / `without` and decides the last 1 to 7 days of recorded Cedar decisions
again. Each one is decided with the request as it was then: the groups, the
device, the mode. It answers the ones that would come out the other way, and
counts the rows it cannot read rather than guessing at them.

## 8. A session's role (P3-2b)

A session takes a role at its start, as an option on the start
(`{device_id, folder, role}`). The vault decides it with the `role:assume`
policies, for the starter, on that device. A session that started with no role
holds none, and nothing in the vault is allowed it.

```mermaid
sequenceDiagram
    participant B as the person's browser
    participant H as hive (the session's owner)
    participant C as core's hook registry
    participant V as vault (SessionGate)
    B->>H: POST …/hive/session {device_id, folder, role}
    H->>H: the start's own gates (membership, HIVE_RUN, the device)
    H->>C: session_gate()
    alt no gate: the vault is not mounted
        C-->>H: none
        H-->>B: refused · vault_unavailable
    else
        C-->>H: the vault's gate
        H->>V: role_assume {user, role, device {os, tags, owner}, purpose}
        V->>V: Cedar role:assume, then RECORD the decision with its request
        V-->>H: allow / deny
        H-->>B: allow: the record holds vault_role, the device is sent the start
        H-->>B: deny: refused · role_refused (nothing made, nothing sent)
    end
```

**Hive never calls the vault.** The module graph has `vault → hive`, so hive
asks through core's hook registry: [`SessionGate`](../crates/core/src/hooks.rs),
which only the vault registers ([`gate.rs`](../crates/modules/vault/src/gate.rs)).
Hive's side is [`vault_gate.rs`](../crates/modules/hive/src/vault_gate.rs).

| When | Who is asked about | The device in the question | Refused as |
|---|---|---|---|
| a start that names a role | the starter | the device it starts on | `role_refused` · `vault_unavailable` · `role_undecided` |
| a promote or a teleport of a session with a role | the person moving it | the device it moves **to** | the same, on the move's answer |
| a fork of a session with a role | the person forking it | the device the fork runs on | the same, on the fork's answer |

> [!WARNING]
> - **A role is decided with its device, again at every move.** A policy may
>   say "on a build box only". A session that moved with its role unchecked
>   would carry the build box's powers to a laptop.
> - **No vault is a refusal, never a pass.** A start or a move that names a
>   role is refused when no vault is mounted to decide it.
> - **The decision is recorded before it is answered.** A decision the vault
>   cannot record is `role_undecided`, and the session does not start or move.

What the gate answers:

| The ask | Answer, and the reason recorded |
|---|---|
| the vault does not serve the organization | deny · `not_served` |
| the role is not one of its live roles | deny · `unknown_role` |
| the device's `os` is not a spelling the rows store | deny · `device: …` |
| otherwise | Cedar's `role:assume`: the policies that decided it, or `default_deny` |

Every decision is a `vault_audit` row with `action: role:assume`, `op` the
purpose (`start` · `promote` · `teleport` · `fork`), `target: role:<name>`,
the session and the device, and the **request** exactly as it was decided. A
fork's row names the session it forks, because the fork has no id until it is
made. `simulate/replay` decides those requests again.

## 9. The device's key (P3-3a)

A value the vault leases to a session will cross to the session's device as a
sealed box (libsodium's `crypto_box_seal`: X25519 and XSalsa20-Poly1305) for a
key that device's daemon holds. P3-3a makes that key and tells the server
about it. No value crosses yet: leases are P3-3b.

```mermaid
sequenceDiagram
    participant D as roomlerd (vault_enabled on)
    participant F as fleet (the agent socket)
    participant R as the device's agents row
    Note over D: the first hello on the primary enrollment's<br/>connection makes an X25519 key, in memory only
    D->>F: rc:agent.hello {…, vault_key: base64(public half)}
    F->>F: vault_key_is_well_formed?<br/>32 bytes, canonical standard base64, not zero
    alt well formed, or none
        F->>R: $set vault_key (every hello, the empty string included)
    else malformed
        F->>R: $set vault_key: "" and a warning
    end
    Note over D,R: a restart makes a new key, and its first hello replaces the stored one
```

| The device | Its hello's `vault_key` | The row's `vault_key` |
|---|---|---|
| `vault_enabled` off (the default) | none | empty |
| on, the primary enrollment's connection | the public half of this run's key | that key |
| on, a secondary organization's connection | none | empty, on that organization's row |
| a build without the `vault` feature, or an agent before P3-3a | none | empty |
| a key that is not well formed | as sent | empty, and the fleet logs a warning |

> [!IMPORTANT]
> - **The key lives in daemon memory, and every start makes a new one.** A
>   leased value lives in daemon memory for its lease only, so a restart ends
>   every lease whatever becomes of the key. A key kept on disk, where the
>   overlay and SSH keys live, would add one thing: a way for whoever reads
>   the config, or a backup of it, to open a sealed value captured earlier.
>   Those two keys are identities a peer pins across restarts; nothing pins
>   this one.
> - **The primary enrollment's connection only.** A key given to two
>   organizations' servers would tie the device's enrollments together
>   ([`multi-org.md`](multi-org.md)), and agent sessions run for the primary
>   organization only.
> - **Never the WireGuard key**, and never derived from it.
> - **The row names the running process.** The fleet writes the key on every
>   hello, the empty string included, so a key never outlives the daemon that
>   made it. The vault will seal only to the key on the row of the device a
>   session runs on.

| Piece | Where |
|---|---|
| the device's switch | `vault_enabled` ([`config.rs:579`](../crates/agent-core/src/config.rs#L579)), off, never pushable: [`the_device_owned_refusals_are_not_pushable`](../crates/remote_control/src/models.rs#L8781) |
| the key, and its half in the hello | [`vault.rs:35`](../agents/roomlerd/src/vault.rs#L35) `hello_key`, feature `vault` (it implies `hive`, and no release build carries it yet); the hello at [`signaling.rs:1570`](../agents/roomlerd/src/signaling.rs#L1570) |
| the wire | `AgentHello::vault_key` ([`signaling.rs:363`](../crates/remote_control/src/signaling.rs#L363)) and [`vault_key_is_well_formed`](../crates/remote_control/src/signaling.rs#L120) |
| what the fleet stores | [`socket.rs:104`](../crates/modules/fleet/src/socket.rs#L104), [`AgentDao::update_hello`](../crates/services/src/dao/agent.rs#L486), `Agent::vault_key` ([`models.rs:2082`](../crates/remote_control/src/models.rs#L2082)); no route answers it |

## 10. Leases (P3-3b)

A session gets a value as a **lease**: released to the daemon that runs it,
sealed to that daemon's key (§9), for at most 15 minutes. The session's device
asks over its own control connection, and the vault answers on that
connection. P3-3b builds the vault's half; a device asks from P3-4, which
brings the daemon's half and the session's tools.

```mermaid
sequenceDiagram
    participant D as roomlerd (the session's device)
    participant V as vault (its agent-socket handler)
    participant DB as session record · device row · vault
    D->>V: rc:vault.lease {req_id, session_id, fence, secret, field, mode, audience, ttl_secs}
    V->>DB: the device's row (advertises vault? holds a key?)
    V->>DB: the session record (on THIS device? at this fence? live? a role?)
    V->>DB: the secret (by path, or this org's RRN), its current version
    V->>V: Cedar secret:use, the device's facts from its ROW
    alt allowed
        V->>V: open the version (KEK), seal it to the row's key
        V->>DB: record the allow, write vault_leases
        V-->>D: rc:vault.leased {req_id, lease_id, n, sealed, expires_at_ms}
    else
        V->>DB: record the deny (once the session is known)
        V-->>D: rc:vault.refused {req_id, reason}
    end
```

| Check, in order | Refused as | Recorded |
|---|---|---|
| the request within its bounds (`lease_limits`), in a mode that releases a stored value: `proxy`, `file`, `env`, `exec` | `bad_request` | no |
| the vault serves the organization | `not_served` | no |
| the device takes leases: its hello advertised `vault` and carried a key | `unsupported` | no |
| a session of the organization whose record places it on THIS device | `not_located` | no |
| at the record's fence | `stale_fence` | yes |
| live: starting, idle, running or awaiting an approval | `ended` | yes |
| within the session's 120 requests a minute | `busy` | no |
| holding a vault role (§8) | `no_role` | yes |
| the secret, by path or by this organization's RRN, live | `no_secret` | yes |
| not disabled | `disabled` | yes |
| its field holding a value | `no_value` | yes |
| a usable KEK | `unavailable` | yes |
| Cedar's `secret:use`, the guardrails included | `denied` | yes, with the request |
| P3-4e — where it denied: a driver's yes to this very request lifts it; otherwise a card waits for one, or a driver said no (§18) | `awaiting_approval` · `denied` | yes, with the request |
| its version not past the `expires_at` its writer gave (P3-3c) | `no_value` | yes, as `version_expired` |
| opened, and sealed to the device's key | `unavailable` | yes |
| P3-4e — the yes that allowed it, spent by compare-and-set | `denied` | yes, as `approval_spent` |
| the allow recorded and the lease written | `undecided` | — |

> [!WARNING]
> - **Believed only from the session's device, at its fence.** The vault reads
>   the session's RECORD: a device cannot lease for a session it does not
>   run, and a run that lost its session to a move cannot lease for it any
>   more. Hive's frames follow the same rule.
> - **The device's facts are its ROW's**, never the request's: a device
>   cannot claim to be a build box, or not to be a Windows machine.
> - **Recorded before released.** The allow is recorded and the lease written
>   before the sealed value is sent; a lease the vault could not record is
>   never released (`undecided`).
> - **`identity`, `short-lived` and `placeholder` release no stored value.**
>   A policy that permits `identity` means "authenticate as the session",
>   and a lease in that mode would hand over the raw value instead, so the
>   vault refuses it as `bad_request`.
> - **Until the session is identified, a refusal is logged, not recorded**:
>   there is no session to name yet, and a device's flood must not become
>   audit rows. The per-session ceiling answers `busy` unrecorded for the
>   same reason.
> - **One word, never a reason that names a policy.** A device is told
>   `denied`; which policy decided is in `vault_audit` for an administrator.

Each connection has its own queue (16 requests), answered in order by one
task, so a slow decision never holds the agent socket's read loop, and one
more request than the queue holds is answered `busy`. A TTL outside
30 s to 900 s is clamped, and a policy reads the clamped value
(`context.ttl_s`), so it can hold a lease shorter still. From P3-3c a lease
also ends at its version's `expires_at` when that comes first, so a policy
that read the asked TTL allowed at least what was granted.

| Piece | Where |
|---|---|
| the wire | `rc:vault.lease` · `rc:vault.leased` · `rc:vault.refused` ([`signaling.rs`](../crates/remote_control/src/signaling.rs)), the refusal words and bounds ([`vault.rs`](../crates/remote_control/src/vault.rs)), `RpcCap::Vault` |
| the handler | [`lease.rs`](../crates/modules/vault/src/lease.rs): `LeaseSocket` (one queue per connection), `lease` (the checks above) |
| sealing | [`crypto.rs`](../crates/modules/vault/src/crypto.rs) `seal_for_device`: libsodium's `crypto_box_seal`, a fresh ephemeral key per box |
| the edge | `vault → hive` (`crates/core/src/graph.rs`): the session record through hive's own stateless DAO |

## 11. Revocation (P3-3c)

A lease ends at its TTL, or at its version's `expires_at` when that comes
first. The vault takes it back sooner once it stops standing, and tells the
device on its own control connection: `rc:vault.revoke {lease_ids, reason}`.
The device drops every lease the frame names, whatever the word (§12).

```mermaid
sequenceDiagram
    participant A as an administrator · fleet's cascade
    participant V as vault (on the device's pod)
    participant DB as vault_leases · session record · secret · device row
    participant D as roomlerd (the session's device)
    alt a value written, a secret disabled or deleted, a device removed
        A->>V: the route, or agent_removed after hive's
        V->>DB: revoked_at + revoked_why (a compare-and-set)
        V-->>D: rc:vault.revoke {lease_ids, reason}, at once when D is on this pod
    else a session's end or move, a new key, the org no longer served
        loop every vault.revoke_sweep_secs, and at each hello of D
            V->>DB: D's unexpired leases, their sessions and secrets, D's row
            V->>DB: revoked_at + revoked_why for each that no longer stands
            V-->>D: rc:vault.revoke for each revoked lease it was not told of
        end
    end
```

| Reason | When | Told |
|---|---|---|
| `session_ended` | the session's record is gone, or past the statuses it is leased in: a stop was ordered, or it ended, was refused or lost | at the next sweep of the device's pod |
| `session_moved` | the record is at another fence, or on another device | at that sweep |
| `value_changed` | the field has a newer version | at once on the device's pod; from another pod, at that pod's next sweep |
| `secret_disabled` · `secret_deleted` | the secret was disabled, or deleted | the same |
| `key_changed` | the device's row holds another key than the one the value was sealed to: its daemon restarted, and the lease died with its memory | at its hello |
| `device_removed` | the device is being removed: fleet's cascade, after hive's in `HOOK_ORDER` | at once, before its row and socket go |
| `not_served` | the vault no longer serves the organization (`vault.tenants`) | at the next sweep or hello |

> [!WARNING]
> - **Each pod looks after the devices connected to it.** It holds their
>   connections (the lease socket's) and checks the leases of those holding a
>   live one every `vault.revoke_sweep_secs` (10 s by default). Nothing crosses
>   pods: a change made on another pod writes the lease's row at once, and the
>   device's own pod tells it at its next sweep. So revocation works with
>   Redis down, and needs nothing from fleet's Hub.
> - **A session is not followed, it is read again.** Hive must never call the
>   vault (`vault → hive`), and a session ends or moves on many paths: its
>   device's report, a stop, a purge, a promote, a teleport, a fork. Each check
>   reads the record and asks whether the lease still stands.
> - **No acknowledgement.** Every hello is told again of each revoked lease that
>   has not expired, so a frame lost to a pod roll, or a device that was away,
>   costs a repeat.
> - **An expired lease is its own revocation**: never taken back, never told.
> - **Told only to a build that can read the frame**: a connection whose row
>   advertises `vault` and holds a key. An older build back on the device is
>   not sent it, and its leases are taken back all the same (its key changed).
> - **A removed device's leases never hold up its removal.** A failure is
>   logged, never the cascade's refusal: a device being removed, possibly a
>   compromised one, must not stay because the vault could not write. What it
>   holds expires within 15 minutes whatever happens.
> - **A version's own expiry is kept.** A version past the `expires_at` its
>   writer gave is never released (`no_value`, recorded `version_expired`),
>   and no lease outlives its version. P3-1 promised this of the first step to
>   use a value; P3-3b did not keep the promise, and P3-3c does.

| Piece | Where |
|---|---|
| the wire | `rc:vault.revoke` ([`signaling.rs`](../crates/remote_control/src/signaling.rs)); the words (`RevokeReason`) and `lease_limits::MAX_REVOKE_IDS`, 64 a frame ([`vault.rs`](../crates/remote_control/src/vault.rs)) |
| the checks | [`revoke.rs`](../crates/modules/vault/src/revoke.rs): `verdict` (pure), `check_device`, `sweep_once`, `reconcile` at a hello, `secret_changed`, `device_removed` |
| the connections | `Revoker` in `revoke.rs`, fed by the lease socket's lifecycle ([`lease.rs`](../crates/modules/vault/src/lease.rs)): a hello links its connection and reconciles the device, a close unlinks it unless a newer connection took over |
| the setting | `vault.revoke_sweep_secs` (`ROOMLER__VAULT__REVOKE_SWEEP_SECS`); 0 or absent is 10 |

## 12. The daemon's lease client (P3-4a)

The device's half of a lease: the daemon asks on its primary connection,
opens what the vault sends with its key (§9), and holds the value in memory
until the lease is over. P3-4a builds the client; P3-4c's tools are the first
to ask.

```mermaid
sequenceDiagram
    participant T as a session's tool (P3-4c)
    participant D as roomlerd (its lease client)
    participant V as vault
    T->>D: a request: session, fence, secret, field, mode, audience, TTL
    D->>V: rc:vault.lease {req_id, …} on the primary connection
    alt leased
        V-->>D: rc:vault.leased {req_id, lease_id, n, sealed, expires_at_ms}
        D->>D: open with this run's key, hold it zeroized
        D-->>T: the lease, never its value
    else refused, or no answer within 15 s
        V-->>D: rc:vault.refused {req_id, reason}
        D-->>T: the vault's word, or no answer
    end
    Note over D: let go at its expiry, at rc:vault.revoke, or when its run ends here
```

| When | The daemon |
|---|---|
| `vault_enabled` on, a build with `vault` that runs sessions, the primary connection | advertises `vault` in its hello and in every re-announcement, exactly where the hello carries its key |
| a tool asks | sends `rc:vault.lease` with a random `req_id`, and waits 15 s: a server without the vault never answers |
| `rc:vault.leased` | opens the box with this run's key, holds the value zeroized, and answers the tool with the lease, never the value |
| `rc:vault.refused` | answers the tool with the vault's word |
| `rc:vault.refused` with `awaiting_approval` (P3-4e-2, §18) | asks again while the card waits: after 2 s, the pause doubling to 15 s, for at most the card's 10 minutes. A lease, any other word, or the card's end is the tool's answer |
| the lease's expiry passes | lets it go: checked at every use, and every 5 s |
| `rc:vault.revoke` (§11) | lets go of every lease the frame names; an id it does not hold is ignored |
| the session's run ends here | lets go of that run's leases, wherever the run stopped being live |
| a lease is let go, whichever way | zeroes its value, removes every file written with it (P3-4c-2, §15), and stops everything served with it: a proxy endpoint and its connections (P3-5b-2, §19) |
| one of these frames on a secondary org's connection | drops it unopened |

> [!WARNING]
> - **A value never leaves the daemon.** A tool borrows it inside a closure
>   (`with_value`) and nothing returns it; it is zeroed when let go.
> - **Held no longer than the longest lease the vault grants** (900 s),
>   whatever the server's clock says: a device whose clock runs behind cannot
>   keep a value past its lease by more than that.
> - **The word follows the key.** The vault answers only a connection that
>   advertises `vault` and whose hello carried a key, so the daemon applies the
>   word from the same inputs as the key, and every re-announcement carries it:
>   the server replaces its stored caps with each.
> - **A run's leases die with the run**, at its fence: another fence of the
>   same session is another run.

| Piece | Where |
|---|---|
| the client | [`vault.rs`](../agents/roomlerd/src/vault.rs): `Client` (`request`, `request_until_approved`, `on_leased`, `on_refused`, `on_revoke`, `drop_run`, `with_value`, `attach`, `stop_with`), `takes_leases`, `apply_word` |
| the frames | their arms in `handle_server_msg` ([`signaling.rs`](../agents/roomlerd/src/signaling.rs)), on the primary connection only |
| a run's end | `Supervisor::unlive` ([`supervisor.rs`](../agents/roomlerd/src/hive/supervisor.rs)), which every way a run stops being live passes through |

## 13. A session's view of the vault (P3-4b)

Before a session's model asks for a value, it needs to know what there is to
ask for: which secrets its role may use, their kinds and fields, where a value
may be presented, and the programs it may be handed to. P3-4b answers that,
and nothing more: a name and its rules, never a value, nor a version's number,
nor who wrote it (`VaultSecretMeta`,
[`vault.rs:99`](../crates/remote_control/src/vault.rs#L99)).

```mermaid
sequenceDiagram
    participant M as the session's model
    participant T as its toolbelt (roomlerd)
    participant V as vault
    M->>T: vault_list {prefix?} · vault_describe {secret}
    T->>V: rc:vault.list {req_id, session_id, fence, prefix} · rc:vault.describe {…, secret}
    Note over V: believed as a lease is (§10): served, the device takes leases,<br/>the session on THIS device at its fence, live, within its ceiling, in a role
    V->>V: Cedar's secret:list for each secret, with the device's facts from its row
    V->>V: recorded before it is answered
    alt allowed
        V-->>T: rc:vault.listed {req_id, secrets, more} · rc:vault.described {req_id, secret}
        T-->>M: the metadata, as the tool's text
    else refused, or no answer within 15 s
        V-->>T: rc:vault.refused {req_id, reason}
        T-->>M: an error the model reads, naming the word
    end
```

| | `vault_list` | `vault_describe` |
|---|---|---|
| Asks for | the live secrets under a folder (`prefix`), or all of them | one secret, by its path or this organization's RRN |
| Decided by | Cedar's `secret:list`, secret by secret; only the permitted are answered | `secret:list`, for that secret |
| Answers | at most 200 ([`MAX_LISTED`](../crates/remote_control/src/vault.rs#L62)), with `more` when a bound left some out; never which were refused, nor how many | its metadata, or `denied` · `no_secret` |
| Recorded | one `vault_audit` row a call: op `list`, `listed N of M`, no request | one row: op `describe`, allow or deny, with the request a replay decides again |

The toolbelt (FR-90 P1a, [`hive.md`](hive.md) §4) offers the two tools only
where the device takes leases: its owner's `vault_enabled` on, a build with
`vault`, the primary connection
([`daemon_vault`](../agents/roomlerd/src/hive/toolbelt.rs#L404)). Wherever it
offers them, the session's settings allow them (`permissions.allow`,
[`launch.rs:410`](../crates/hive-node/src/launch.rs#L410)), so no person is
asked to approve each one: they only read, and their gate is the vault's own
policy, decided and recorded by the server. A call the harness cancels gets
no answer, and the daemon stops waiting for it.

> [!IMPORTANT]
> - **Listing is its own permission.** A role allowed to use a secret lists
>   nothing until a policy lets it list, and listing uses nothing: neither
>   action implies the other (§7). Neither guardrail stops a listing.
> - **A listing never says what it left out**, only that a bound did
>   (`more`). A narrower prefix finds the rest.
> - **Believed exactly as a lease is**: the same identity checks in the same
>   order, the same refusal words and the same per-session ceiling
>   ([`identify`](../crates/modules/vault/src/lease.rs#L686)), so a device that
>   floods the vault with listings is refused as one flooding it with leases
>   is.
> - **Offered and allowed together.** The daemon decides once, at a run's
>   launch, whether its toolbelt offers the tools, and writes the settings'
>   allow from the same answer
>   ([`supervisor.rs:4209`](../agents/roomlerd/src/hive/supervisor.rs#L4209)).
>   It records whether the primary connection takes leases before it resumes
>   any run ([`signaling.rs:1617`](../agents/roomlerd/src/signaling.rs#L1617)).

| Piece | Where |
|---|---|
| the frames | `rc:vault.list` · `rc:vault.describe` ([`signaling.rs:1262`](../crates/remote_control/src/signaling.rs#L1262)) and `rc:vault.listed` · `rc:vault.described` ([`:4308`](../crates/remote_control/src/signaling.rs#L4308)); refusals reuse `rc:vault.refused` |
| the decisions | `list` and `describe` ([`meta.rs:107`](../crates/modules/vault/src/meta.rs#L107)); `secret:list` in [`policy.cedarschema`](../crates/modules/vault/src/policy.cedarschema) and `Ask::ListSecret` ([`policy.rs:380`](../crates/modules/vault/src/policy.rs#L380)) |
| the daemon's asks | `Client::list` and `Client::describe` ([`vault.rs:465`](../agents/roomlerd/src/vault.rs#L465)) |
| the tools | `vault_list` and `vault_describe` ([`toolbelt.rs:835`](../agents/roomlerd/src/hive/toolbelt.rs#L835)), answered by `vault_tool` ([`toolbelt.rs:972`](../agents/roomlerd/src/hive/toolbelt.rs#L972)) |

## 14. A command with values: `vault_exec` (P3-4c-1)

The first tool that USES a value. A session asks for one command to run with
values from the vault in its environment. The daemon leases each value for
that program, runs the command as the session's account, and answers with its
output, every value scrubbed. The model sees the result, never the values.

```mermaid
sequenceDiagram
    participant M as the session's model
    participant A as a person (approve)
    participant T as its toolbelt (roomlerd)
    participant V as vault
    M->>T: vault_exec {argv: ["psql", …], env: {PGPASSWORD: "db/main#password"}}
    T->>A: approve? (the settings ask for every vault_exec)
    A-->>T: allow
    T->>T: argv[0] a bare name, found in the system's own directories, root's
    T->>V: rc:vault.lease {mode: exec, audience: "psql"}, one per env entry
    Note over V: secret:use, with the guardrails: exec-only-allowed-programs,<br/>windows-proxy-only, prod-never-materializes
    V-->>T: rc:vault.leased (sealed to the daemon's key)
    T->>T: run as the session's account, from its folder: a clean environment + the values
    T->>T: kill the command's group, let every lease go, scrub every value
    T-->>M: {exit_code, stdout, stderr, truncated, timed_out}, each value as [vault:NAME]
```

| Step | What holds | Where |
|---|---|---|
| the call | `argv`: 1 to 64 strings, `argv[0]` a program's **bare** name; `env`: 1 to 8 `NAME: handle` (a path or this org's RRN, `#field` for a structured secret's field); `timeout_s` 1 to 300, default 60. Never `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `IFS`, `ENV`, `BASH_ENV`, nor `LD_*` / `DYLD_*`: those would steer what runs | [`vault_exec.rs:154`](../agents/roomlerd/src/hive/vault_exec.rs#L154) |
| the program | looked for only in `/usr/local/sbin`, `/usr/local/bin`, `/usr/sbin`, `/usr/bin`, `/sbin`, `/bin`, and refused unless root (or this daemon) owns it and neither group nor other can write it: a program the session's account could replace is never handed a value | [`vault_exec.rs:213`](../agents/roomlerd/src/hive/vault_exec.rs#L213) |
| the leases | one per `env` entry, mode `exec`, audience the program's bare name, TTL the timeout + 30 s; all asked before anything runs, and one refusal runs nothing | [`vault_exec.rs:421`](../agents/roomlerd/src/hive/vault_exec.rs#L421) |
| the run | as the session's account (decision 13: no administrator groups, on Linux no privilege gained by exec), from the session's folder, in its own process group, stdin closed, a clean environment (`HOME`, `USER`, `LOGNAME`, `PATH` = those directories, `LANG`) plus the values | [`vault_exec.rs:302`](../agents/roomlerd/src/hive/vault_exec.rs#L302) |
| the end | at its exit, its timeout, or the call cancelled: the whole group killed and every lease let go, each by a guard that runs however the call ends | [`vault_exec.rs:260`](../agents/roomlerd/src/hive/vault_exec.rs#L260), [`:489`](../agents/roomlerd/src/hive/vault_exec.rs#L489) |
| the answer | the exit code (and signal), stdout and stderr, at most 64 KiB each; each value replaced by `[vault:NAME]`, the longest first, then the daemon's own redactor (agent tokens, bearer and JWT shapes). A value shorter than 4 bytes cannot be masked without shredding the output, and the answer names it under `not_scrubbed` | [`vault_exec.rs:387`](../agents/roomlerd/src/hive/vault_exec.rs#L387) |

**`exec_allow` is a guardrail.** `guardrail:exec-only-allowed-programs`
([`policy.rs:110`](../crates/modules/vault/src/policy.rs#L110), §7) forbids
every `exec` lease whose audience is not on the secret's `exec_allow`. The
`Secret` entity carries `exec_allow`, so the refusal is a Cedar decision:
`simulate` names it, it is recorded with its request, and no organization can
drop it. A request recorded before the attribute existed reads back naming no
program. `exec_allow` holds bare names only
([`program_name_ok`](../crates/remote_control/src/vault.rs#L87), the one
definition the vault's write check
([`routes.rs:375`](../crates/modules/vault/src/routes.rs#L375)) and the
device share): a path could match nothing a device asks for.

> [!WARNING]
> - **`exec` is a materializing mode** (design §8.5): the program holds the
>   value, and a program that prints or sends it can leak it. Hence the
>   permit, `exec_allow`, and a person approving every call.
> - **Scrubbing is a seatbelt, not a boundary.** A value printed in another
>   encoding (base64, split across lines) passes it.
> - **Windows runs nothing.** The vault refuses `exec` there first
>   (`windows-proxy-only`), and the refusal is in `vault_audit` (AC13). A
>   Windows daemon never runs a materialized value even if a lease came back.
> - **Homebrew's programs are refused** on macOS: `/opt/homebrew` belongs to
>   a person, who could replace what a value is handed to.

> [!IMPORTANT]
> **A person approves every use because the settings ASK for it.** Keeping
> it out of the allow list is not enough. The session's settings list
> `vault_exec` and `vault_render` under `permissions.ask`
> ([`VAULT_USE_TOOLS`](../agents/roomlerd/src/hive/toolbelt.rs#L390), written
> by [`vault_use_tools`](../agents/roomlerd/src/hive/supervisor.rs#L4675)).
> - **Why an ask:** a repository's own `.claude/settings.json` loads too (the
>   FR-90 spec's open decision 7), and an allow there would skip the person.
>   Claude Code reads deny, then ask, then allow, across every settings file,
>   so an ask prompts whatever another file allows. Until this was found,
>   while `vault_render` was written, the uses were only kept out of `allow`.
> - ⚠️ **Not by marking the tools as requiring user interaction**
>   ([`launch.rs:420`](../crates/hive-node/src/launch.rs#L420)). A
>   `--permission-prompt-tool` can never approve such a tool: Claude Code
>   turns its allow into a deny, and `approve` is that tool.

| Piece | Where |
|---|---|
| the tool | `vault_exec` in [`toolbelt.rs:868`](../agents/roomlerd/src/hive/toolbelt.rs#L868), offered with the reads where the device takes leases, and asked for in the settings |
| where it runs | the run's `ExecTarget`, made at launch from the session's account, home and folder ([`supervisor.rs:4243`](../agents/roomlerd/src/hive/supervisor.rs#L4243)); none on Windows |
| letting go | `Client::release` ([`vault.rs:621`](../agents/roomlerd/src/vault.rs#L621)) |

## 15. A file with values: `vault_render` (P3-4c-2)

Some programs read a credential only from a file: a kubeconfig, an `.env`, a
`.pgpass`. A session asks for a template to be written with values from the
vault into its secrets directory, and gets the file's path, never a value. The
file lives exactly as long as its leases.

```mermaid
sequenceDiagram
    participant M as the session's model
    participant A as a person (approve)
    participant T as its toolbelt (roomlerd)
    participant V as vault
    M->>T: vault_render {template: "user={{ db/main#user }}\npass={{ db/main#password }}", dest: "db.env"}
    T->>A: approve? (the settings ask for every vault_render)
    A-->>T: allow
    T->>V: rc:vault.lease {mode: file, audience: "db.env"}, one per secret
    V-->>T: rc:vault.leased, each
    T->>T: render, write <runtime>/<session>/secrets/db.env (0600, the account's)
    T->>T: attach the file to every lease: the first let go removes it
    T-->>M: {path, ttl_s}
```

| Step | What holds | Where |
|---|---|---|
| the call | `template`: at most 64 KiB, each secret as `{{ handle }}` (a path or this org's RRN, `#field` for a field), 1 to 8 secrets; a `{{` never closed is refused, never kept as text. `dest`: a bare file name, by the rule `exec_allow` holds. `ttl_s`: 30 to 900, default 300 | [`vault_render.rs:98`](../agents/roomlerd/src/hive/vault_render.rs#L98), [`:62`](../agents/roomlerd/src/hive/vault_render.rs#L62) |
| the leases | one per distinct secret, mode `file`, audience the file's name, all asked before anything is written; one refusal writes nothing and lets the others go | [`vault_render.rs:253`](../agents/roomlerd/src/hive/vault_render.rs#L253) |
| the directory | `<runtime>/<session>/secrets/`, made by the daemon, its own, `0711`: the session's account opens a file whose name it knows, and lists nothing. A link or a file in its place is refused, never followed | [`vault_render.rs:170`](../agents/roomlerd/src/hive/vault_render.rs#L170) |
| the file | rendered into a buffer sized once (a vector that grew would leave copies of a value behind), written whole: a temporary made with `create_new`, `0600`, handed to the account, renamed into place | [`vault_render.rs:143`](../agents/roomlerd/src/hive/vault_render.rs#L143), [`:192`](../agents/roomlerd/src/hive/vault_render.rs#L192) |
| its life | attached to each of its leases, so it is removed the moment the first is let go: at its expiry, at `rc:vault.revoke`, or when the run ends here. The leases are kept, not released, when the call returns | [`vault.rs:202`](../agents/roomlerd/src/vault.rs#L202), [`:628`](../agents/roomlerd/src/vault.rs#L628) |
| a restart | the daemon removes every `secrets/` an earlier process left: no lease outlives the process that held it | [`supervisor.rs:725`](../agents/roomlerd/src/hive/supervisor.rs#L725), [`vault_render.rs:234`](../agents/roomlerd/src/hive/vault_render.rs#L234) |
| the Read tool | the session's settings deny it the secrets directory where the vault tools are offered | [`supervisor.rs:4636`](../agents/roomlerd/src/hive/supervisor.rs#L4636) |

> [!WARNING]
> - **`file` is a materializing mode** (design §8.5). The session's account
>   reads the file, which is what it is for, so a Bash command can print it.
>   The Read tool's deny is UX. The gates are the permit, the guardrails
>   (`prod-never-materializes` refuses a production secret in `file` mode) and
>   a person approving every call.
> - **Windows writes nothing.** The vault refuses `file` there
>   (`windows-proxy-only`), recorded, and a Windows daemon never writes a
>   materialized value even if a lease came back.
> - **A file with several values dies with its shortest lease.**

| Piece | Where |
|---|---|
| the tool | `vault_render` in [`toolbelt.rs:899`](../agents/roomlerd/src/hive/toolbelt.rs#L899), offered with the reads, and asked for in the settings as `vault_exec` is (§14) |
| keeping the leases | `Releasing::keep` ([`vault_exec.rs:497`](../agents/roomlerd/src/hive/vault_exec.rs#L497)) |

## 16. A lease asked for ahead: `vault_request` (P3-4c-3)

A session can ask for a lease before it uses one. It learns early that the
vault refuses, or uses one value in several commands without asking each
time. The lease is held in daemon memory and never handed to the session. A
later `vault_exec` or `vault_render` that names the same secret toward the
same audience takes it.

```mermaid
sequenceDiagram
    participant M as the session's model
    participant T as its toolbelt (roomlerd)
    participant V as vault
    participant A as a person (approve)
    M->>T: vault_request {handle: "db/main#password", mode: exec, audience: "psql", ttl_s: 600}
    Note over T: allowed in the settings: it hands the session nothing
    T->>V: rc:vault.lease {mode: exec, audience: "psql"}
    Note over V: secret:use, with every guardrail, recorded
    V-->>T: rc:vault.leased: held, marked asked for ahead
    T-->>M: {lease_id, handle, mode, audience, ttl_s}
    M->>T: vault_exec {argv: ["psql", …], env: {PGPASSWORD: "db/main#password"}}
    T->>A: approve? (the settings ask for every use, §14)
    A-->>T: allow
    T->>T: the lease asked for ahead covers the run: taken, nothing asked again
    T-->>M: the output, each value scrubbed
```

| Step | What holds | Where |
|---|---|---|
| the call | `handle`: a path or this org's RRN, `#field` for a field. `mode`: `exec`, `file` or (P3-5b-2) `proxy`, the modes a use takes. `audience`: a bare name, by the rule `exec_allow` holds; the program `vault_exec` will run, or the file `vault_render` will write; for `proxy`, the upstream `vault_connect` will sign in to, `host:port` by that tool's own rule (§19). `ttl_s`: 30 to 900, default 300 | [`vault_request.rs:34`](../agents/roomlerd/src/hive/vault_request.rs#L34) |
| the lease | asked for once, decided by `secret:use` with every guardrail, recorded, and held in daemon memory marked as asked for ahead; the answer names it and how long it lasts, never its value | [`vault_request.rs:88`](../agents/roomlerd/src/hive/vault_request.rs#L88), [`vault.rs:149`](../agents/roomlerd/src/vault.rs#L149) |
| its use | `vault_exec` takes it when it names the same secret and field for the same program, and the lease covers the command's whole timeout. `vault_render` takes it when it names the same secret toward the same file, and the lease lasts at least the file's `ttl_s`. Otherwise a call asks for its own, as before | [`vault.rs:657`](../agents/roomlerd/src/vault.rs#L657), [`vault_exec.rs:448`](../agents/roomlerd/src/hive/vault_exec.rs#L448), [`vault_render.rs:271`](../agents/roomlerd/src/hive/vault_render.rs#L271) |
| its end | its expiry, `rc:vault.revoke`, or the run's end: no call lets it go. A file written with it goes with it (§15), so a render's `ttl_s` answers how long the file truly lives: until the first of its leases ends | [`vault_render.rs:300`](../agents/roomlerd/src/hive/vault_render.rs#L300) |
| the settings | allowed, with the reads. The vault decides and records it, and it hands the session nothing. Each use of the lease still asks a person (§14) | [`supervisor.rs:4662`](../agents/roomlerd/src/hive/supervisor.rs#L4662) |

> [!IMPORTANT]
> **A use takes the lease by what it is, not by its id.** The design had the
> use tools take a lease by its id. But a call naming `lease:…` would hide
> from the person approving it which secret it hands on. So a call still
> names the secret, and the daemon finds the lease that matches: the same
> run, secret, field, mode and audience, lasting long enough. A lease a call
> asked for itself is never taken by another.

> [!NOTE]
> The vault cannot tell a lease asked for ahead from one a use asks for: it
> decides and records both as any `secret:use`. Being asked for ahead lives
> only on the device.

| Piece | Where |
|---|---|
| the tool | `vault_request` in [`toolbelt.rs:946`](../agents/roomlerd/src/hive/toolbelt.rs#L946), offered with the reads, allowed in the settings with them ([`VAULT_AHEAD_TOOLS`](../agents/roomlerd/src/hive/toolbelt.rs#L399)) |
| finding the lease | `Client::find_ahead` ([`vault.rs:657`](../agents/roomlerd/src/vault.rs#L657)); how long one has left, `Client::left` ([`vault.rs:678`](../agents/roomlerd/src/vault.rs#L678)) |

## 17. What a device reports doing: `vault_activity` (P3-4d)

The vault's own decisions are in `vault_audit` (§20). What a device says it
DID with its leases is a different claim, made by a host that may be
compromised, so it is kept apart in `vault_activity`, as `ssh_activity` is
kept apart from `ssh_audit` ([`roomler-ssh.md`](roomler-ssh.md)). P3-4d-1
built the server's half: the frame, the check, the collection and the read.
P3-4d-2 builds the device's: its owner's `vault_activity_log` switch, and a
report after each command `vault_exec` ran and each file `vault_render`
wrote.

```mermaid
sequenceDiagram
    participant D as device (roomlerd)
    participant V as vault
    participant A as an administrator
    D->>V: rc:vault.activity {session_id, fence, kind: exec, lease_ids, target: "psql", exit_code}
    Note over V: fire-and-forget: no answer, on the connection's own queue
    V->>V: each lease released to THIS device, for this session,<br/>in the use's mode, toward its target?
    alt every one
        V->>V: kept in vault_activity, never vault_audit
    else any other
        V->>V: dropped, at debug
    end
    A->>V: GET …/vault/activity ?session_id= · ?lease_id=
    V-->>A: the reports, newest first
```

| Step | What holds | Where |
|---|---|---|
| the switch | the device's own `vault_activity_log`, default off, read at each primary hello. Never pushable: like `ssh_activity_log`, it is the device's word about itself, and a test holds every remote configuration to leaving it out | [`config.rs:588`](../crates/agent-core/src/config.rs#L588), [`config_surface.rs:509`](../crates/agent-core/src/config_surface.rs#L509), [`models.rs:8847`](../crates/remote_control/src/models.rs#L8847) |
| the report | after a `vault_exec` that ran its command (the program, and its exit code or its timeout) and a `vault_render` that wrote its file (its name), naming every lease the use took, asked for there or ahead. A call refused before anything ran or was written reports nothing. Fire-and-forget: a full connection drops the report rather than hold the call | [`vault.rs:344`](../agents/roomlerd/src/vault.rs#L344), [`vault_exec.rs:474`](../agents/roomlerd/src/hive/vault_exec.rs#L474), [`vault_render.rs:307`](../agents/roomlerd/src/hive/vault_render.rs#L307) |
| the frame | `rc:vault.activity {session_id, fence, kind, lease_ids, target, exit_code?, timed_out?}`. `kind` is `exec` (a command `vault_exec` ran: the program as the target, and how it ended) or `render` (a file `vault_render` wrote: its name as the target); 1 to 8 leases. Fire-and-forget, like `rc:ssh.activity`: no answer, and an older server ignores it | [`signaling.rs:1297`](../crates/remote_control/src/signaling.rs#L1297), [`vault.rs:287`](../crates/remote_control/src/vault.rs#L287) |
| the queue | the connection's own lease queue: a flood is dropped at its bound like a flood of requests, and each connection's reports are handled in order. At most 120 a minute per session on its device | [`lease.rs:361`](../crates/modules/vault/src/lease.rs#L361) |
| the check | every lease the report names was released to this device, for this session in this organization, in the use's mode (`exec` for `exec`, `file` for `render`) and toward its target. Otherwise, or for a kind this server cannot name or a target that is not a bare name, the report is dropped | [`lease.rs:385`](../crates/modules/vault/src/lease.rs#L385), [`dao.rs:570`](../crates/modules/vault/src/dao.rs#L570) |
| the row | the device from the socket, the session, the kind, the leases, the target, and for a command its exit code and whether it timed out (never kept for a file), with when it arrived. 90 days | [`model.rs:291`](../crates/modules/vault/src/model.rs#L291), [`lib.rs:320`](../crates/modules/vault/src/lib.rs#L320) |
| the read | `GET …/vault/activity`, administrators only, newest first: `?session_id=` for one session's, `?lease_id=` for those naming one lease, which is where a use joins its decision | [`routes.rs:1035`](../crates/modules/vault/src/routes.rs#L1035) |

> [!WARNING]
> - **A claim, not a decision.** A compromised device can report a use that
>   never happened, or leave one out. The lease's decision in `vault_audit` is
>   what survives a device that lies or goes quiet.
> - **An empty answer is not evidence of disuse.** A device reports only while
>   its owner's `vault_activity_log` is on, and that defaults off. A server
>   can never make a device describe itself honestly; the switch says so out
>   loud.

## 18. Approvals: the yes a policy asks for (P3-4e)

A policy can allow a use only with a person's approval:
`when { context has approval }`, or, naming whose yes counts,
`context.approval.approver in Roomler::Group::"admin"`. P3-4e-1 builds the
vault's half. When Cedar denies a lease that a yes could allow, the vault
posts a card into the session's room and answers `awaiting_approval`. One of
the session's drivers answers the card, and the session's next ask for the
same thing is leased. P3-4e-2 builds the device's half: a tool whose lease
waits on a card asks again until the answer, so the session's call simply
takes longer, and the model is told plainly when a person said no or nobody
answered. P3-4e-3 builds the card's answer in the web app: under the card's
text, where it stands, and **Approve** / **Deny** for a driver who may answer it
now ([`ui.md`](ui.md)). Any other chat client shows the card's text, and its
route answers it.

```mermaid
sequenceDiagram
    participant D as device (roomlerd)
    participant V as vault
    participant R as the session's room
    participant P as a driver
    D->>V: rc:vault.lease {…, mode: env, audience: psql, ttl_secs: 300}
    V->>V: Cedar secret:use: deny
    V->>V: would the yes of a driver who may answer allow it?
    V->>R: a card, posted as the session, pushed to its drivers
    V-->>D: rc:vault.refused {reason: awaiting_approval, approval_id}
    D->>V: the same ask again
    V-->>D: awaiting_approval, the same card
    P->>V: POST …/vault/approval/{id} {answer: approve}
    V->>V: Cedar again, with the driver as the approver: allow
    V->>R: the card: approved by the driver
    D->>V: the same ask again
    V->>V: Cedar with the approver: allow · the yes spent (compare-and-set)
    V-->>D: rc:vault.leased
    V->>R: the card: approved, and used
```

| Cedar denied a lease, and the vault finds for this very request | It answers |
|---|---|
| a driver's yes, usable, for at least the TTL asked | Cedar decides again with them as the approver; an allow spends the yes and releases the lease |
| a card waiting for an answer | `awaiting_approval`, naming that card: no second card |
| a driver's no, within the card's deadline | `denied` |
| nothing, and the yes of a driver who may answer now would allow it | a new card, and `awaiting_approval` |
| nothing, and no driver's yes would | `denied`, as before P3-4e |

"This very request" is one session at its fence on its device, one secret's
field, one mode and one audience, for no longer than the TTL the card named.
A yes to `env` toward `psql` for five minutes is no yes to `file`, to another
program, or to fifteen minutes.

Whose yes counts is the policy's to say, as anything else is. Four eyes, for
instance: a session's owner may not approve their own session's use, and any
other driver may.

```cedar
forbid (principal, action == Roomler::Action::"secret:use", resource)
when { context has approval && context.approval.approver == principal.owner };
```

The vault then posts a card only when the session has a driver besides its
owner.

| Piece | What holds | Where |
|---|---|---|
| the refusal | `awaiting_approval`, retryable. `rc:vault.refused` carries `approval_id` with it, and with no other word | [`vault.rs:162`](../crates/remote_control/src/vault.rs#L162), [`signaling.rs:4282`](../crates/remote_control/src/signaling.rs#L4282), [`lease.rs:671`](../crates/modules/vault/src/lease.rs#L671) |
| the decision | after Cedar's deny: a usable yes, a waiting card, a no, or a new card where a driver's yes would allow it | [`lease.rs:854`](../crates/modules/vault/src/lease.rs#L854), [`approval.rs:93`](../crates/modules/vault/src/approval.rs#L93) |
| the approver | Cedar reads `context.approval.approver` in a `Group` per roomler role they hold, as a person assuming a role is. A recorded request keeps the groups (`approver_groups`), so a replay decides it as it was decided | [`policy.rs:354`](../crates/modules/vault/src/policy.rs#L354), [`policy.rs:675`](../crates/modules/vault/src/policy.rs#L675), [`policy.rs:541`](../crates/modules/vault/src/policy.rs#L541) |
| who may answer | a driver who reads the session, while it is live: hive's own rule for its approvals. Their yes must be one the policy accepts, decided by Cedar with them as the approver when it is given, and again when a lease would spend it. A card is posted only where some driver could give one | [`access.rs:70`](../crates/modules/hive/src/access.rs#L70), [`approval.rs:151`](../crates/modules/vault/src/approval.rs#L151), [`approval.rs:622`](../crates/modules/vault/src/approval.rs#L622) |
| the card | a message the session authors in its room, bound to `{module: "vault", ref: <approval id>}` and pushed to its drivers as hive's own approvals are. It names the secret's handle, the mode, the audience, the TTL and the deadline, and is edited as the approval moves on: approved, used, denied, expired, withdrawn | [`room.rs:654`](../crates/modules/hive/src/room.rs#L654), [`approval.rs:286`](../crates/modules/vault/src/approval.rs#L286) |
| the routes | `GET …/vault/approval/{id}`: the approval as the session's readers see it, with `may_answer` (the caller drives it now) and `may_approve` (and the policy would take their yes) for the caller. `POST …/vault/approval/{id}` `{answer: "approve" \| "deny"}` | [`approval.rs:595`](../crates/modules/vault/src/approval.rs#L595), [`approval.rs:622`](../crates/modules/vault/src/approval.rs#L622) |
| one yes, one lease | the lease spends the yes by compare-and-set (`approved` → `used`) before its allow is recorded | [`lease.rs:942`](../crates/modules/vault/src/lease.rs#L942), [`dao.rs:816`](../crates/modules/vault/src/dao.rs#L816) |
| the windows | a card waits 10 min for an answer (`APPROVAL_ANSWER_SECS`); a yes is usable for 5 min from the answer (`APPROVAL_USE_SECS`) | [`vault.rs:75`](../crates/remote_control/src/vault.rs#L75) |
| the device's wait (P3-4e-2) | `awaiting_approval` reaches the lease client with its approval's id, and a tool's lease (`vault_exec`, `vault_render`, `vault_request`) asks again while the card waits: after 2 s, the pause doubling to 15 s, for at most the card's 10 minutes. Each ask is a decision the vault records, which is why the pause grows. A lease goes on as any lease does; a `denied` tells the model a driver may have said no; a card nobody answered in time names its approval and says to ask again once a driver can answer a new card. A cancelled tool call stops the asking | [`vault.rs:420`](../agents/roomlerd/src/vault.rs#L420), [`vault.rs:576`](../agents/roomlerd/src/vault.rs#L576), [`toolbelt.rs:417`](../agents/roomlerd/src/hive/toolbelt.rs#L417), [`toolbelt.rs:500`](../agents/roomlerd/src/hive/toolbelt.rs#L500) |
| the web app (P3-4e-3) | under a message bound to `{module: "vault"}`, the approval as `GET …/vault/approval/{id}` gives it: where it stands, the deadline while it waits, and **Approve** / **Deny** for a driver who may answer it now, **Approve** only where the policy would take their yes (`may_approve`, else off with why). A refusal is shown in the server's words. The vault edits the card's message whenever the approval moves on, and the new `updated_at` reads it again. The buttons only reflect what the route says; every decision stays the server's | [`VaultApprovalCard.vue`](../ui/src/components/vault/VaultApprovalCard.vue), [`MessageBubble.vue`](../ui/src/components/chat/MessageBubble.vue) |
| the row | `vault_approvals`, one per card: its request, its status (`pending` · `approved` · `denied` · `used` · `expired` · `withdrawn`), who answered and when, the lease that spent it, and the card's message. A unique index holds one pending card per request | [`model.rs:341`](../crates/modules/vault/src/model.rs#L341), [`lib.rs:333`](../crates/modules/vault/src/lib.rs#L333) |

| The answer route says | When |
|---|---|
| 200, the approval | a driver's answer, recorded |
| 400 | an answer that is neither `approve` nor `deny` |
| 403 | the caller does not drive the session now, or the policy does not accept their yes |
| 404 | no such approval in the organization, or the caller does not read its session: the same answer for both |
| 409 | answered already, or used; past its deadline (now `expired`); its session moved or ended (now `withdrawn`); its secret or its device gone since |

> [!WARNING]
> - **A card is a stub.** It names the secret's handle, the mode, the
>   audience and the TTL, which the vault holds anyway, and never why the
>   session asked: that step is in the transcript, which streams from a
>   replica (design §5.2). The audience is the session's own word, so the
>   card escapes it.
> - **One yes, one lease.** The compare-and-set comes before the allow is
>   recorded: a lost race releases nothing, and a failure after it only ever
>   wastes a yes. The next ask needs a new one.
> - **A yes is decided twice**: when it is given, and when a lease spends it.
>   A policy or a role that changed in between allows nothing.
> - **A card belongs to its session at its fence, on its device.** One past
>   its deadline expires, and one whose session moved or ended is withdrawn,
>   when it is next read or answered. A new card for the same request first
>   ends a stale one, which would otherwise hold the request's one pending
>   place in the unique index.
> - **Only drivers answer**, as with hive's own approvals. A policy narrows
>   whose yes counts among them; it cannot let anyone else answer. There is no
>   approver role outside the drivers: such a person is named a driver first.

## 19. `proxy` mode (P3-5)

In `proxy` mode the daemon serves an endpoint that authenticates upstream
with the leased value, so the session, its model and its transcript never
hold the value (AC12). A secret's `audiences` say which upstreams it may
reach (§7). The design is the spec's §3e, "P3-5, `proxy` mode, as designed".
P3-5b-1 builds the first engine, Postgres's, in `roomlerd`'s module
`vault_proxy` (`agents/roomlerd/src/vault_proxy/mod.rs`), and P3-5b-2 serves
it: `vault_connect` and its endpoint, below.

```mermaid
sequenceDiagram
    participant C as client (psql)
    participant P as the proxy (roomlerd)
    participant U as upstream (Postgres)
    C->>P: SSLRequest
    P-->>C: N (the endpoint is a local socket)
    C->>P: startup {user: anyone, database}
    Note over P: replication refused<br/>user := the secret's
    P->>U: TCP, SSLRequest
    U-->>P: S
    Note over P,U: TLS, the certificate verified
    P->>U: startup {user: the secret's, database}
    U-->>P: AuthenticationSASL [SCRAM-SHA-256, …]
    P->>U: client-first (a nonce)
    U-->>P: server-first (its nonce, salt, iterations)
    P->>U: client-final (a proof, never the password)
    U-->>P: server-final (its signature)
    Note over P: the signature checked: only<br/>a server holding the password makes it
    U-->>P: AuthenticationOk
    P-->>C: AuthenticationOk, unasked
    C->>U: every byte, both ways, through P
```

| Step | What the engine does | Where |
|---|---|---|
| the client's `SSLRequest`, `GSSENCRequest` | answered `N`: the endpoint is a local socket | `agents/roomlerd/src/vault_proxy/pg.rs:100` |
| its `CancelRequest` | sent to the upstream as it came, over TLS: the key it names is the upstream's own, relayed unchanged | `pg.rs:107`, `pg.rs:318` |
| its startup | `replication` refused, each one it names (Postgres takes the last, so a "no" first is no cover); `user` replaced by the secret's; everything else kept | `pg.rs:119`, `pg.rs:136` |
| the upstream | TCP, an `SSLRequest` it must accept, and TLS verified against the system's roots and the secret's `ca` | `pg.rs:282`, `agents/roomlerd/src/vault_proxy/tls.rs:20` |
| its authentication | SCRAM-SHA-256 only, and the server's signature checked before the session starts; bound to the certificate the proxy verified where the upstream offers `-PLUS` (P3-5b-3) | `pg.rs:177`, `pg.rs:189`, `pg.rs:195`, `pg.rs:238` |
| what it says meanwhile | a protocol negotiation passed on at once, since a client takes one only before its `AuthenticationOk`; a notice held until after it | `pg.rs:256`, `pg.rs:263` |
| then | `AuthenticationOk` to the client, and every byte both ways | `pg.rs:272` |

> ⚠️ **The client never authenticates.** The endpoint is its credential: a
> socket only the session's account can reach (P3-5b-2). The client is never
> asked for a password, and the secret's never crosses the endpoint, in
> either direction.

**What it refuses**, each before the password could leave:

| What | The refusal says |
|---|---|
| an upstream that declines TLS | "the upstream declined TLS" (`pg.rs:305`) |
| a certificate that no root and no `ca` vouches for | "TLS to *host*: …" |
| a cleartext or MD5 password asked for, or Kerberos, GSSAPI or SSPI | "the upstream asked for …; the proxy authenticates with SCRAM-SHA-256 only" |
| SASL without SCRAM-SHA-256 | "the upstream offers no SCRAM-SHA-256" |
| a server that signs wrong: it never held the password | "… it does not hold the password" |
| a server that began SCRAM and lets the session in unsigned | "… without proving it holds the password" |
| a client asking for a replication connection | refused before any upstream is dialled |

The proxy's refusals reach the client as Postgres writes a fatal error
(`FATAL`, SQLSTATE `08004`), saying why (`pg.rs:409`). An upstream's own
error, such as a wrong password or an unknown database, is relayed as the
upstream wrote it.

**SCRAM-SHA-256** (RFC 5802, RFC 7677), the client's half
(`agents/roomlerd/src/vault_proxy/scram.rs`):

| Rule | Why | Where |
|---|---|---|
| the password SASLprepped (RFC 4013) | both ends derive the key from the same form: a no-break space is a space, a control character is refused | `scram.rs:108` |
| the server's nonce must extend this exchange's | a server first message from another exchange is refused | `scram.rs:145` |
| at most 10,000,000 iterations | past that, an exchange is a way to make the daemon burn CPU (Postgres and MongoDB default to 4096 and 15000) | `scram.rs:33`, `scram.rs:155` |
| a mandatory extension (`m=`) refused | as the RFC requires of a client that does not know it | `scram.rs:140` |
| the server's signature compared in constant time | a server that cannot sign never had the password | `scram.rs:185`, `scram.rs:199` |
| bound to the channel (P3-5b-3) | where the upstream offers `-PLUS`, `p=tls-server-end-point`: the certificate the proxy verified, hashed with the hash its signature names (RFC 5929: MD5 and SHA-1 become SHA-256), inside what the proof covers. Where it offers none, `y`. `n` only where the certificate defines no binding data (Ed25519, RSASSA-PSS) | `scram.rs:38`, `tls.rs:125` |

> ⚠️ **What binding catches.** Inside verified TLS, SCRAM proves that both
> ends hold the password; binding adds the certificate the proxy saw to
> what the proof covers. Behind a TLS terminator the system also trusts (an
> inspection proxy whose CA is in the OS store), the real upstream compares
> the binding with its own certificate and refuses ("SCRAM channel binding
> check failed"). A relay that strips `-PLUS` from the offer meets the
> proxy's `y`, which an upstream that binds refuses as a downgrade ("SCRAM
> channel binding negotiation error"). Both refusals are the upstream's,
> relayed in its words. An upstream that cannot bind (an older server, a
> pooler) takes `y`, so nothing that worked stops working.
>
> ⚠️ **TLS is never unverified.** A proxy authenticates with a value, and a
> value presented to an upstream nobody verified is presented to whoever
> answered. The system's roots are trusted (a corporate CA in the OS store
> included, as the daemon's own connection trusts it), and so is the
> secret's `ca` when it holds one (`tls.rs:22`, `tls.rs:26`). Only `ring` is
> linked: `tokio-rustls` without its default `aws-lc-rs`.

**Bounds.** A session has 30 s to start, both sides together, and an
upstream has 10 s to accept a connection (`pg.rs:50`, `pg.rs:52`). A
client's startup packet is at most 10,000 bytes (Postgres's own limit), and
an upstream's message before the session at most 64 KiB (`pg.rs:46`,
`pg.rs:48`).

**Tested** against a fake upstream over real TLS, with a certificate made
for each test:
- the whole road, with a client that sends no password;
- the upstream's own refusal, relayed;
- each refusal above, with what the upstream saw after it searched for the
  password;
- a trusting upstream, told nothing;
- a cancel;
- the order of a negotiation and a notice;
- RFC 7677's exchange, byte for byte;
- (P3-5b-3) the exchange bound where the upstream offers `-PLUS`, and `y`
  where it offers none; a stripped offer and a relayed channel, each
  refused by the upstream; the binding's hash for P-256 and P-384
  certificates made in the test, none for Ed25519 or RSASSA-PSS, and SHA-1
  made SHA-256.

### The endpoint and `vault_connect` (P3-5b-2)

`vault_connect` is the toolbelt's tool for `proxy` mode. The daemon leases the
secret's `user` and `password` toward the upstream, serves the engine above on
a socket only the session's account can reach, and answers how to connect,
with no credential anywhere in the answer
(`agents/roomlerd/src/hive/vault_connect.rs`).

```mermaid
sequenceDiagram
    participant M as the session (Claude Code)
    participant T as its toolbelt (roomlerd)
    participant V as vault
    participant E as the endpoint (roomlerd)
    participant U as upstream (Postgres)
    M->>T: vault_connect {handle, engine, upstream, ca?}
    Note over M,T: asked of a person first, as every use is
    T->>V: lease #user, #password (and the CA), proxy mode, toward host:port
    Note over V: proxy-only-named-audiences:<br/>an upstream the secret names, exactly
    V-->>T: the leases, sealed to the device's key
    T->>E: bind proxies/<id>/.s.PGSQL.5432, the account's, 0600
    T-->>M: host (the directory), port, uri, env, ttl_s: no value
    M->>E: psql host=<directory>, any user, no password
    Note over E: the peer's uid is the session account's
    E->>U: TLS verified, SCRAM-SHA-256 with the values read from the leases now
    E-->>M: AuthenticationOk, then a session as the secret's user
    Note over E: the first lease let go closes the endpoint<br/>and every connection it holds
```

| Step | What holds | Where |
|---|---|---|
| the call | `handle`: a structured secret, by its path or this org's RRN, with no `#field`. `engine`: `postgres`. `upstream`: `host:port`, exactly as the secret's `audiences` names it (a host name, an IPv4 address or a bracketed IPv6 one; one spelling per port). `database`: optional, named in the connection string. `ca`: optional, a secret whose value is the upstream's CA (PEM). `ttl_s`: 30 to 900, default 300 | `vault_connect.rs:141`, `vault_connect.rs:99` |
| its place | at most 8 endpoints per run, taken before the vault is asked, so a call past it asks the vault nothing | `vault_connect.rs:293` |
| the leases | the secret's `user` and `password`, and the CA's, each in `proxy` mode toward the upstream: `proxy-only-named-audiences` refuses an upstream the secret does not name (§7), recorded. A lease `vault_request` asked for ahead toward the same upstream (§16) is taken as it is | `vault_connect.rs:434` |
| the values | read once to check them (a user Postgres takes; a CA that parses), then again from the leases as each connection starts, never kept by the endpoint | `vault_connect.rs:335` |
| the endpoint | `<runtime>/<session>/proxies/<id>/.s.PGSQL.5432`: both directories the daemon's (`0711`), the socket handed to the session's account (`0600`). A Postgres client dials it as `host=<that directory>`, port 5432 | `vault_connect.rs:375` |
| a connection | from the session's account only (its peer uid checked again, root's refused too), at most 16 at once, one engine session each | `vault_connect.rs:575` |
| its life | the first of its leases to be let go (its expiry, `rc:vault.revoke`, the run's end) cancels the endpoint's stop, which closes its socket and every connection it holds. The leases the call asked for itself are then let go too, rather than held to their expiry. An earlier process's endpoints go at the daemon's start | `vault_connect.rs:491`, `vault_connect.rs:613`, `vault_connect.rs:232` |
| the answer | `host`, `port`, `database`, a `uri` (`postgresql:///<db>?host=<dir>&port=5432`) and `env` (`PGHOST`, `PGPORT`, `PGDATABASE`), `ttl_s`: no `PGPASSWORD`, no value | `vault_connect.rs:531` |

> [!WARNING]
> - **The settings ASK for every call.** `vault_connect` hands the session no
>   value, but the endpoint acts as the secret's user, so it is asked for with
>   the other uses (`permissions.ask`, §14), and a repository's own settings
>   cannot allow it past the person.
> - **What closes an endpoint is the lease client's own let-go.** The daemon's
>   lease client cancels the stop token of every endpoint served with a lease
>   whenever it lets that lease go (`stop_with`, §12), as it removes a rendered
>   file (§15). A connection therefore never outlives its lease.
> - **Windows serves no endpoint yet.** Its database clients mostly cannot dial
>   a Unix socket, so a call there is refused before anything is leased.
>   P3-5b-4 serves loopback TCP, each connection's process identified.
> - **A use is not reported yet.** `vault_activity` (§17) takes `exec` and
>   `render`; a `proxy` endpoint's opening is a later step's report.

## 20. Audit

`vault_audit` holds every `secret:*` and `vault:verify` decision, allow and
deny. Each row has the principal (`user` in P3-1), the action as a policy
names it, the operation, the decision and its reason (`administrator` or
`not_administrator`), and the RRN, field and version number where there is
one. P3-2a adds every `policy:write` (a role's or a policy's), with its
`target`. P3-2b adds every `role:assume` (§8), with the session, the device
and the request it decided. P3-3b adds every lease decision once its session
is identified (§10): `secret:use`, op `lease`, principal `session` (the row's
user is the session's owner), the session, the device, the request a Cedar
decision decided, and a `target` of `lease:<id>` on an allow, or of
`asked:<name>` when the name resolved to no secret. P3-3c adds every
revocation (§11): op `revoke`, decision `revoke`, principal `vault`, the
reason word, and a `target` of `lease:<id>`; one row per lease, written by the
pod whose compare-and-set took it back. P3-4b adds every `secret:list` (§13),
principal `session`: op `describe`, allow or deny, with the secret and the
request a replay decides again; op `list`, one row a call, the reason
`listed N of M` and a `target` of `asked:<prefix>` (`asked:*` for all), with
no request, since an aggregate is not a decision a replay could make again.
P3-4c-1's `exec` leases are `secret:use` rows like any lease, the program as
the request's audience; an `exec` refused for its program names
`guardrail:exec-only-allowed-programs` as its reason (§14). P3-4c-2's `file`
leases are recorded the same way, the file's name as the audience (§15).
P3-4c-3's leases asked for ahead are recorded as any lease is (§16).
P3-4e adds the approvals (§18). A lease denied while a card waits is a
`secret:use` deny with the reason `awaiting_approval`, and one a driver said
no to has `approval_denied`; each has a `target` of `approval:<id>` and its
request. A lease a yes allowed records the approver and their groups in its
request, and one whose yes was spent in a race is refused as
`approval_spent`. Each answer is a row of its own: op `approval`, principal
`user` (the person answering), the reason `approved` or `denied`, or a
refusal (`not_a_driver`, `approver_refused: …`), a `target` of
`approval:<id>`, and no request: the lease a yes allows is the decision a
replay decides again.
Rows expire after
90 days, like the other audits.

- **An allow is recorded before it is acted on, and fails closed.** A decision
  the vault cannot record is answered 500 and not acted on.
- A refusal is recorded too, on a best-effort basis: the caller is refused
  whatever the database says.
- The status and the audit read are not decisions about a secret, and are not
  recorded.
- A request refused for its content (400, 422) or for want of a KEK (503) acts
  on nothing, so it records no allow.
- A device's own report of using a value is a different claim, made by a host
  that may be compromised. It goes to `vault_activity` (§17), never here, as
  `ssh_audit` and `ssh_activity` are kept apart.

## 21. Collections

| Collection | One row per | Indexes | Retention |
|---|---|---|---|
| `vault_secrets` | secret | unique `(tenant_id, path)` over live rows (partial on `deleted_at: {$type: "null"}`, so a live row stores an explicit `null`); `(tenant_id, deleted_at, path)` | tombstoned, kept |
| `vault_versions` | value written | unique `(secret_id, field, n)`; `(tenant_id, kek)` | the last 10 per field; all go with their secret |
| `vault_audit` | decision | `(tenant_id, at desc)`, `(secret_id, at)` | 90 days (TTL on `at`) |
| `vault_roles` | vault role (P3-2a) | unique `(tenant_id, name)` over live rows | tombstoned, kept |
| `vault_policies` | policy (P3-2a): one Cedar statement, its effect, a revision | unique `(tenant_id, name)` over live rows; `(tenant_id, deleted_at, created_at)` | tombstoned, kept |
| `vault_leases` | value released (P3-3b): the secret, field and version; the session, its fence, its owner and its device; the key it was sealed to (`sealed_to`); the mode and audience; issued and expiry times; `revoked_at`/`revoked_why` from P3-3c (§11). Never the value | `(tenant_id, session_id)`; P3-3c `(device_id, expires_at)` (a pod's checks, a hello) and `(secret_id, expires_at)` (a change to a secret) | a week past `expires_at` (TTL); the decision stays in `vault_audit` for 90 days |
| `vault_activity` | a device's report of a use (P3-4d, §17): the device, the session and its fence, the kind (`exec` · `render`), the leases, the target, an exec's exit code and timeout. A claim, never a decision | `(tenant_id, at desc)`, `(tenant_id, session_id, at desc)`, `(lease_ids)` | 90 days (TTL on `at`) |
| `vault_approvals` | approval card (P3-4e, §18): its request (the session, its fence and device, the secret's field, the mode, the audience, the TTL), its status, who answered and when, the lease that spent it, the card's message | unique `(session_id, fence, device_id, secret_id, field, mode, audience)` over pending rows; `(session_id, secret_id, requested_at desc)` | 90 days (TTL on `requested_at`) |

P3-3a adds no collection: a device's key is a field on fleet's `agents` row
(`vault_key`, §9). Neither do P3-4a to P3-4c-3: a lease the daemon holds,
asked for ahead or not, lives in its memory (§12, §16), a listing is a
decision recorded in `vault_audit`, and a rendered file is on the device, for
its lease (§15).

## 22. What comes next

| Step | Builds |
|---|---|
| P3-5b-4 | the Windows endpoint: loopback TCP, each connection's process identified and refused unless it runs as the session's account |
| P3-5c to P3-5f | `proxy` mode's other engines, as designed in the spec (§3e): MongoDB, HTTP named upstreams, the Kubernetes API and the SSH agent |
| P3-6 to P3-9 | `short-lived` credentials, session SOCKS, `ssh_run` and `fleet_exec` as tools, `roomler connect` |
| P3k | the field run in the test organization |
