---
paths:
  - "crates/core/**"
  - "crates/modules/**"
  - "crates/api/**"
  - "crates/db/src/indexes.rs"
  - "crates/remote_control/src/signaling.rs"
  - "crates/tests/src/composition_tests.rs"
  - "crates/tests/fixtures/composition.baseline.json"
  - "ui/src/stores/capabilities*"
  - "Cargo.toml"
---

# Modular monolith (FR-69) — the rules that bind new server code

<!-- Moved verbatim from CLAUDE.md on 2026-10-28, when that file had grown to 58 KB and was
     loaded on every prompt and into every subagent. CLAUDE.md keeps a four-line summary. This file loads only when Claude reads
     or edits a file matching `paths:` (Claude Code path-scoped rules). Keep it under 40,000
     characters: from Claude Code 2.1.309 an on-demand file is cut there. Block HTML comments like
     this one are stripped before the model sees the file. -->

Full treatment: `docs/modular-monolith.md`.

### Modular monolith (FR-69, shipped 2026-09-04) — the rules that still bind

Full treatment: **`docs/modular-monolith.md`**. What constrains new work:

1. **The DAG is data** (`crates/core/src/graph.rs`). Any module → core;
   `conference → chat`, `remote → fleet`, `network → fleet`, `hive → fleet`, `hive → chat`. **Core never calls a
   module** — the inverse flows are hooks core invokes in `hooks::HOOK_ORDER`
   (session holders → lease holders → the record owner), and a failing holder
   STOPS the cascade. A hook is the *only* way a module reaches "up".
2. **The composition is gated by a byte-identical baseline.**
   `crates/tests/src/composition_tests.rs` asserts every route with its allowed
   methods, the index plan for both `multi_block` values, and every wire name
   against `crates/tests/fixtures/composition.baseline.json`. Re-record **only**
   with `COMPOSITION_UPDATE=1` and a commit message that says why — a reviewer
   diffs the JSON against the claim.
3. **`ensure_indexes` is `index_plan(multi_block)` applied.** A module's index sets
   go in the module, never in `crates/db/src/indexes.rs`; a spec outside the plan
   is invisible to the gate.
4. **The wire does not move.** `ClientMsg`/`ServerMsg` stay in
   `remote_control/src/signaling.rs`, and every variant names an owner via an
   exhaustive `namespace()`. ⚠️ **The prefix is NOT the owner**: `rc:consent*` is
   fleet's; `rc:relay.*` and `rc:agent.key_rotated` are network's.
   ⚠️ A **defensive `_ =>` arm added before the variants that would make it
   reachable** fails `clippy -D warnings` with `unreachable_patterns`; annotate it
   `#[allow(unreachable_patterns)]` with a comment, and remove the allow when the
   variants land (`docs/modular-monolith.md` §6).
5. **A module built on another declares it as `Module::Deps`** — `remote`'s is
   `FleetState`, because the Hub is ONE live object and a module that re-created it
   would dispatch into an empty one. ⚠️ A *stateless* dependency (a DAO over
   `core.db`, a pure guard) is **not** a `Deps` — re-create it.
6. **An unmounted module answers 503, never a boot refusal.** `[modules] x = false`
   unmounts; the gauges read zero. ⚠️ A view that reads two modules where **both**
   are required belongs to the one that depends on the other; a view where one is
   **optional** is the HOST's — putting the device listing in `network` on the
   strength of a graph edge made every `remote` profile 404 its own devices page,
   which a `/health`-only boot smoke could not see.
7. **A profile is a feature aggregate, and what it leaves OUT is the claim.**
   `/health` and `/api/capabilities` report the **mounted** set plus `compiled` —
   never `graph::MODULES`, which every build knows and would make a `mesh` image
   answer all six. ⚠️ An absence assertion that cannot fail proves nothing: the
   `profiles` CI job checks each reduced profile with `--no-default-features` **and**
   a positive control on the full graph.
8. **The SPA gates on `/api/capabilities`, failing OPEN until the server answers**
   — the server enforces every action anyway, so the worst case of failing open is
   a link whose page 404s, while failing closed blanks the product behind one
   round-trip. ⚠️ Unknown module names from a newer server are IGNORED, never an
   error.

⚠️ **`State<Core>` is the handler seam.** `AppState` derefs to `roomler_core::Core`,
so a handler or helper that needs only core fields takes `State<Core>` / `&Core`.
`impl FromRef<AppState> for Core` lives in `crates/api/src/core_state.rs` — the ONE
place the orphan rules allow it. `roomler-core` must never learn `AppState` exists.

**Naming**: server crates `roomler-ai-*`; the core `roomler-core` (`crates/core`);
modules `roomler-ai-mod-<name>` (`crates/modules/<name>`); the daemon's shared crate
`roomler-node-core` (`crates/agent-core`, which held the name `roomler-core` from
FR-21 until FR-69 — its pre-FR-21 name is retired, never bring it back).

### Module map (detail)

```
  core/             → package `roomler-core` (AGPL, SERVER side, never linked by an agent):
                      the `Module` trait, hooks, jobs, the module DAG, the composition
                      snapshot, and `Core` itself (`state.rs`) with the /ws registry,
                      dispatcher, Redis fan-out, cluster identity, storage, rate limiting
  modules/saas/       → Stripe, the public updates list + newsletter, plan compliance.
                        An ADD-ON feature, never part of a profile
  modules/chat/       → rooms + membership, messages, reactions, files, search, xlsx
                        export, Giphy, `typing:*`
  modules/conference/ → the mediasoup SFU, media claim-or-route, call lifecycle +
                        recordings, `media:*`. The ONLY crate that links `mediasoup`
  modules/fleet/      → device management: agents, enrollment, presence, the agent Hub,
                        crash/log ingest, releases, owner consent, remote config, fleet
                        RPC, the ONE device-removal sequence, the agent SOCKET + UPGRADE
  modules/remote/     → remote desktop as what a CONTROLLER reaches: session routes,
                        `/turn/credentials`, `/relay/regions`, the `rc:*` controller
                        dispatch, the cross-pod RC relay. Built ON `fleet`
  modules/network/    → pillar 2's server side: the overlay engine (IPAM, netmaps,
                        leases, L3 ACL, relay grants), the org relay mint, the DERP ACL
                        cache, seven route files, `/derp`, the tunnel-client socket.
                        Built ON `fleet`
  modules/hive/       → FR-90 agent sessions, server side: the session record + lifecycle,
                        `rc:hive.*`, `hive_audit` — never session content. Built ON
                        `fleet` and `chat` (a session IS a secret room, written to only
                        as notes + turn stubs); its `[modules]` switch defaults OFF
  modules/vault/      → FR-90 P3 secrets: each value sealed under its own data key,
                        wrapped to an `age` KEK; RRNs; `vault_audit`. No route answers a
                        value. On core alone so far; switch defaults OFF (`docs/vault.md`)
  modules/knowhow/    → FR-90 P4 the organization's map: entities, the links between them,
                        `knowhow_history`. A map, not a gate: it grants nothing. On core
                        alone so far; switch defaults OFF (`docs/knowhow.md`)
```
