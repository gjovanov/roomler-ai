# Roomler — the mission, and the model tier this work requires

**Read this block first, on every task. It is prefixed to every prompt on purpose.**

## 1. What Roomler is

Roomler is **one open-source product that replaces three** — Tailscale, TeamViewer
and Microsoft Teams — on one daemon and one web app:

- **Remote desktop & control from any browser**, to any of your machines anywhere
  in the world, with nothing to install on the controlling side.
- **A secure private WireGuard network** between those machines — the mesh that
  the remote desktop, SSH and tunnels all ride on. End-to-end encrypted; the
  server coordinates and **never carries plaintext**.
- **Team collaboration on top of it** — video conferencing, chat, rooms, files.

Three products, one binary, one identity, one install. That packaging is the
product thesis, not an implementation detail: the reason the mesh exists is so
the remote desktop and the meeting work from anywhere, and the reason they are
one daemon is that a customer should not have to operate three of them.

## 2. The acceptance bar is "it just works"

Not on a clean lab network — on the networks people actually have. Every cell
below is a shipped, field-verified configuration, not an aspiration:

| Axis | Must hold across |
|---|---|
| **Host** | unmanaged personal laptop · GPO-locked corporate desktop (no admin rights, EDR, TLS-inspecting middlebox) · bare server · VM |
| **Network** | home NAT · CGNAT · symmetric-NAT corporate LAN · consumer VPN · enterprise full-tunnel VPN that reroutes and reaps routes underneath us |
| **Platform** | Windows · Linux · macOS — desktop, headless, containers, WSL, and k8s / OpenShift cluster nodes with no session at all |
| **Session** | attended desktop · locked screen · SYSTEM / root service · nobody logged in |

A change that works on the dev box and breaks a corporate laptop behind a
full-tunnel VPN **has not worked**. **CI green ≠ done** — networking and
remote-desktop changes are proven on the real fleet (`roomler exec`,
`roomler ssh`) across those topologies after every roll.

## 3. Model tier — this work runs on Fable, and stays on Fable

Any task touching **remote desktop / control**, **the WireGuard overlay, NAT
traversal, relays, DERP or tunnels**, or **WebRTC video conferencing** is
top-tier work. Do not route it to a weaker model — not Opus 4.8, not Sonnet, not
Haiku — and do not quietly spend a smaller effort budget on it. Where a router, a
subagent spawn or a cost heuristic is choosing the tier for this repository, the
answer is **the strongest model available, at maximum effort**.

That is not a preference. It is the shape the bugs in this codebase actually have:

1. **The cause sits three layers below the symptom.** "This host lost DNS" was a
   leaked WebRTC UDP socket twelve hours earlier that had eaten the entire
   ephemeral port range, while `ping 1.1.1.1` stayed at 3 ms.
2. **One change spans every layer at once** and is wrong in exactly one of them —
   Rust async internals, kernel routing and netlink, Windows WFP and ConPTY, an
   SFU's ICE candidate set, a browser's jitter buffer, BSON serialisation, inside
   a single feature.
3. **The invariants are non-obvious, load-bearing, and each was paid for in the
   field.** `Some([])` means *deny* while `None` means *no policy compiled*.
   Pooling an overlay address before the tombstone locks the next joiner out
   permanently. `ssh` is a prefix of `ssh-consent` and the two mean different
   things, so the match must stay equality. Every one of those reads as a harmless
   simplification to a model that is pattern-matching instead of reasoning — and
   every one was a live production incident.
4. **Most of this surface is unreachable by tests.** The corporate laptop, the
   full-tunnel VPN, the symmetric NAT, the locked screen, the headless cluster
   node — none of it is in CI. The reasoning done *before* the code ships is the
   verification budget.
5. **The blast radius is a fleet.** The daemon runs as SYSTEM/root on every
   enrolled machine and updates itself. A wrong answer does not fail a test — it
   takes an operator's remote access to their own box, freezes fleet updates
   fleet-wide, or opens a silent pivot into a corporate network.

Cheaper is more expensive here. A plausible-but-wrong change costs a release
cycle, a field-debug session across three time zones, and — measured repeatedly in
this repository — weeks of latency before anyone notices it regressed.

> *This block states the requirement; it does not enforce it. The deterministic
> levers are `/model` in-session, `"model"` in `.claude/settings.json`, and
> `CLAUDE_CODE_EFFORT_LEVEL` (already `max` in this repo's settings).*

---

# CLAUDE.md

Guidance for Claude Code working in this repository.

> **This file is the always-loaded tier: every session and every subagent pays for
> it.** It holds what must be true before you open anything — commands, the shape of
> the code, the rules that bind every change — plus pointers. Each pillar's invariants
> are a path-scoped rule in **`.claude/rules/`** that loads itself when you touch that
> code; depth lives in **[`docs/README.md`](docs/README.md)** (the index of every
> engineering doc) and in the skills **`encoder-cells`** and **`ship-it`**.

<!--
Maintainer note. Block HTML comments are stripped before the model sees this file, so
this note costs no context; whoever opens the file to edit it still reads it.

What belongs here: a fact a session needs BEFORE it reads any code. That means commands,
orientation, the rules that bind every change (security, tenancy, the FR process), and
the ops signatures a fleet-debugging session needs without opening code. Everything else
goes elsewhere:
  - .claude/rules/<topic>.md  invariants tied to code paths. The `paths:` frontmatter
                              makes the rule load on Read/Edit of a matching file. Keep
                              each under 40,000 chars. A rule whose YAML does not parse
                              silently loads on EVERY prompt.
  - docs/*.md                 mechanics, history, field logs.
  - a skill                   procedures (release, encoder cells, fleet tests).
Budget: keep this file under ~25 KB. It had grown to 58 KB by 2026-10-28, carrying stale
counts, a fixed issue and an FR's field log. An FR's field results belong in its spec and
issue, never here. Counts that drift (tests, docs, routes) don't belong here either.
§1–§3 above are mirrored in the operator's user-level CLAUDE.md, so edit them together.
-->

## Project Overview

| # | Pillar | What it is | Start here |
|---|---|---|---|
| 1 | **Remote desktop** | TeamViewer-class remote control, in Rust | `docs/remote-control.md` |
| 2 | **Networking** | a WireGuard overlay mesh (Tailscale-class) **+** userspace tunnels (ngrok-class), DERP relays, roomler SSH | `docs/overlay-communication.md`, `docs/tunnels.md` |
| 3 | **Collaboration** | chat · video conferencing · file sharing · rooms | `docs/real-time.md`, `docs/ui.md` |

Two invariants, load-bearing in both directions:

- **One daemon per enrolled machine.** `roomlerd` *is* the remote-desktop target,
  the tunnel exit, the tunnel client, the overlay node and the SSH server — not
  four cooperating services. This is why "just add another install" is almost
  always wrong (fixed TUN name + GUID, a singleton LocalAPI pipe, host-global
  exit/DNS/WFP state, one updater — `docs/multi-org.md`).
- **The server coordinates but never carries plaintext.** Pixels, keystrokes, SSH
  bytes and tunnel payloads travel P2P or over a relay that only ever sees
  ciphertext. Any design that would make the control plane a data path is wrong on
  those grounds alone, not merely on performance grounds.

Stack: Rust (Axum) + MongoDB + Vue 3/Vuetify 3 + Pinia + Mediasoup (WebRTC SFU) +
webrtc-rs (P2P remote-control) + WireGuard/smoltcp (overlay). The agent ships as a
separate native binary (`roomlerd`) that runs on controlled hosts.

**Pillar 2's design goal** (a private network that "just works", at maximum
performance and lowest latency, on real-world networks) and its six commitments are in
`.claude/rules/networking.md`. Read it before touching networking code.

## Commands

```bash
# Development
cargo run --bin roomler-ai-api         # backend (port 3000)
cd ui && bun run dev                   # Vite dev server (port 5000, proxies to 5001)
cd ui && bun run build                 # production UI build (includes vue-tsc --noEmit)

# Agent (native binary — runs on the controlled host)
cargo build -p roomlerd --release --features full      # capture + encode + input (SW encoder)
cargo build -p roomlerd --release --features full-hw   # + Windows MF encoder, WGC capture, viewer indicator
cargo build -p roomlerd --release                      # signalling-only (no media, no input)
./target/release/roomlerd enroll --server <url> --token <enrollment-jwt> --name <label>
./target/release/roomlerd run [--encoder software|hardware]
./target/release/roomlerd encoder-smoke --encoder hardware   # offline MFT/FFmpeg init diagnosis
./scripts/dev-xvfb.sh                  # capture smoke test via a virtual framebuffer

# Testing
cargo test -p roomler-ai-tests         # integration (MongoDB + Redis; see Test Setup)
cd ui && bun run test:unit             # Vitest
cd ui && bun run e2e                   # Playwright

# Static analysis — what CI runs
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo clippy -p roomler-ai-api -p roomler-ai-services -p roomler-ai-tests --all-targets -- -D warnings
cd ui && vue-tsc --noEmit

# Dependency audit
cargo audit / cargo outdated ;  cd ui && bun audit / bun outdated

# Infrastructure
docker compose up -d                   # MongoDB (27019), Redis (6379), MinIO (9000), coturn
```

⚠️ **`cargo clippy --workspace` compiles the feature UNION and can never see a
profile or a feature-gated backend.** `--features full` is a *separate* CI clippy
step — the only one that compiles the capture backend. Run
`cargo check -p roomler-ai-tunnel-core --features overlay-l3,overlay-netstack --all-targets`
in WSL before pushing; a `#[cfg(windows)]` helper needs the same gate on its test.

⚠️ `--workspace` clippy compiles only `pub mod fixtures` from `crates/tests` (every
test module is `#[cfg(test)]`-gated), so `Checking roomler-ai-tests` in a build log
is **not** evidence the tests build.

**Agent build deps**: on Linux, `--features full` (its `media` = `scrap-capture` +
`openh264-encoder`, plus `enigo-input`) needs
`sudo apt install -y libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev`. OpenH264 compiles
from C source on the first build. The no-feature build compiles on any `rust:bookworm`
image. It gives a signalling-only agent for CI that is **not usable in production** (no
capture, no input).

**Encoders**: preference resolves **CLI `--encoder` > env `ROOMLERD_ENCODER` >
`encoder_preference` in config TOML > `Auto`**. Everything else is in the
**`encoder-cells` skill** + `docs/encoders.md`: the child-process capability probe, the
cell matrix, the denylist, the per-backend FFmpeg traps. Load the skill before touching
`agents/roomlerd/src/encode/`.

## Architecture

```
crates/
  config/             → Settings (env vars via ROOMLER__ prefix)
  db/                 → MongoDB models + indexes (native driver, no ORM)
  services/           → auth, DAOs, export, background tasks, OAuth, push, email, Giphy, Claude AI
  remote_control/     → the wire: signalling, consent, audit, TURN creds, ACL rule shapes
                        (agent-side users set default-features = false)
  localapi/           → LocalAPI protocol leaf crate (re-exported as tunnel_core::localapi)
  agent-core/         → roomler-node-core: daemon-free agent building blocks (the companion deps THIS)
  tunnel-core/        → tunnels + the overlay data plane, shared by the CLI and the agent
  derp-relay/         → the standalone regional DERP relay
  tcp-turn-conn/      → TURNS-over-TLS-over-TCP for UDP-blocked networks
  hive-node/          → FR-90 Hive on the device
  roomler-setup-core/ → shared install-wizard machinery
  vendored/           → patched forks (scrap, webrtc*, rtp, wintun-bindings)
  core/               → roomler-core (AGPL, server only): Module trait, hooks, jobs, the module
                        DAG, composition, Core (/ws registry, Redis fan-out, storage, rate limits)
  modules/<name>/     → roomler-ai-mod-<name>: chat · conference (the only mediasoup crate) ·
                        fleet (devices, agent Hub, releases, consent, remote config, fleet RPC) ·
                        remote + network (both on fleet) · hive · vault · knowhow (FR-90,
                        default OFF) · saas (an add-on)
  api/                → Axum HTTP/WS server; compose.rs is the host composition
  tests/              → integration tests (real servers; drive the agent in-process)
agents/
  roomlerd/           → the native agent: WebRTC peer, capture, encode, input, overlay, tunnels, SSH
  roomler-cli/        → tunnel client, bin `roomler` (the command surface lives in the LIB)
  roomler-cli-shim/   → installed by the MSI/.deb as `roomler`; re-execs `roomlerd cli`
  roomler-desktop/    → the tray companion (devices, tunnels, settings, consent)
  roomler-setup/      → Tauri 2 unified install wizard (lib `wizard_app`)
ui/src/               → api/ · components/ · composables/ · stores/ · views/ · plugins/ (docs/ui.md)
scripts/              → dev-xvfb.sh · e2e-*.sh · name-audit.sh · fr-*.sh · anchor-*.sh · signing/
```

**Crate dependency flow**: `config` ← `db` ← `remote_control` ← `services` ← `api`;
`tests` depends on `api` + `config` + `db` + `roomlerd`.

**Modular monolith (FR-69).** The server is `roomler-core` plus modules, composed in
`crates/api/src/compose.rs`.
- Core never calls a module. The inverse flows are hooks.
- The composition is gated by a byte-identical baseline
  (`crates/tests/fixtures/composition.baseline.json`): every route, the index plan,
  every wire name. Re-record it only with `COMPOSITION_UPDATE=1` and a commit message
  that says why.
- An unmounted module answers 503, never a boot refusal.
- Handlers take `State<Core>`.

The eight rules and the module map are in `.claude/rules/modular-monolith.md` +
`docs/modular-monolith.md`.

**Naming**: server crates `roomler-ai-*`, modules `roomler-ai-mod-<name>`, the core
`roomler-core` (`crates/core`), the daemon's shared crate `roomler-node-core`
(`crates/agent-core`). A retired name is never brought back (FR-21, FR-46).

## Multi-Tenancy

All data is scoped by `tenant_id`. Routes nest
`/api/tenant/{tenant_id}/room/{room_id}/message/...`; `tenant_members` tracks
user↔tenant membership, `room_members` room membership.
⚠️ **`is_member(tid)` is not an authorization check for anything keyed by id**. See
Security Baseline.

## Auth Pattern

JWT (jsonwebtoken 9) + Argon2. Access token 7 d, refresh 30 d, configurable via
`ROOMLER__JWT__*` (`SECRET`, `ISSUER`). OAuth via Google, Facebook, GitHub, LinkedIn,
Microsoft. Four `TokenType`s share one secret: `Access`/`Refresh` (users),
`Enrollment` (single-use, 10 min, bootstraps an agent) and `Agent` (long-lived, on the
agent WS). ⚠️ Audience checks are load-bearing: `verify_agent_token` rejects a user
JWT and vice versa (`crates/services/src/auth/mod.rs::tests`).

**`/ws` multiplexes roles.** `role=agent` validates the agent audience; no `role` (or
`role=user`) takes the user flow. `/ws` and `/derp` take an optional
`tid=<tenant-hex>` that the LB hashes on. Agent and tunnel tokens must match their
`tenant_id` claim, users must be members (403 otherwise), and an absent `tid` is a
legacy client that is accepted.

## Route Pattern

```rust
let room_routes = Router::new()
    .route("/", get(routes::room::list))
    .route("/", post(routes::room::create))
    .route("/{room_id}", get(routes::room::get));

Router::new()
    .nest("/api/tenant/{tenant_id}/room", room_routes)
    .with_state(state)
```

Full route inventory: **`docs/api.md`**.

## DB Model Pattern

MongoDB native driver, BSON documents, no ORM. Models live in `crates/db/src/models/`,
except the three remote-control entities in `crates/remote_control/src/models.rs`.
Every collection, index, TTL and ER diagram: **`docs/data-model.md`**. A module's
indexes belong in the module's `index_plan`, never in `crates/db/src/indexes.rs`.
⚠️ The unique composite index on `agents.{tenant_id, machine_id}` makes re-enrolling a
known machine reuse its row.
⚠️ Tombstoned rows use `index_unique_partial(..., {deleted_at: {$type: "null"}})`.
Use `$type`, **not** `{deleted_at: null}`, which also matches *absent*.

## Frontend Conventions

Plugin order i18n → vuetify → pinia → router; Pinia setup stores; TipTap v3 for rich
text; mediasoup-client for WebRTC; `ui/src/api/client.ts` injects the auth token; Vite
proxies `/api` + `/ws` to `:5001`. The SPA gates on `/api/capabilities` and fails
OPEN until the server answers, because the server enforces every action anyway. Layout,
store map and responsive padding: **`docs/ui.md`**.

## Test Setup

Suites and harnesses: **`docs/testing.md`**. The lane's assertions and the E2E rig:
`.claude/rules/testing.md`.

- **Integration** (`crates/tests/`): a database per test, real servers on random
  ports; needs MongoDB `:27019` + Redis `:6379`. Run with **`RUST_MIN_STACK=8388608`**:
  some tests build seven servers, and without it a green suite dies with
  `has overflowed its stack`. Add `RUST_LOG` + `--nocapture`, or refusals are silent.
  CI runs this lane on master pushes touching `crates/**` or `agents/roomlerd/**`, on
  PRs touching `crates/tests/**`, and on dispatch. It does not run on every PR.
- **E2E** (`ui/e2e/`, Playwright): set `E2E_API_URL` (default `:5001`); Mailpit specs
  need `E2E_MAILPIT_URL`. Against a deployment the browser is the **`pwrunner` sidecar
  in the app pod**, and `ROOMLER__APP__FRONTEND_URL` must equal its origin (port
  included) or `/ws` is refused 403.
- **Unit**: Vitest (`ui/src/`), plus in-crate tests in `remote_control`, `roomlerd`
  (CLI tests under `--lib` as `app::tests`) and `services::auth`.

## Environment

`.env` for development (gitignored). Config via `ROOMLER__`-prefixed env vars
(double-underscore separator). `docker-compose.yml` runs MongoDB 7, Redis 7, MinIO
and coturn locally; default DB URL `mongodb://localhost:27019` (tests use no auth).

## Deployment

**Production** is `https://roomler.ai/`, the enrollment `--server` and the controller
origin. Topology and media-path rules: **`docs/deployment.md`** +
`docs/multi-pod-scale-out.md`, in full in `.claude/rules/deployment.md`. Release,
signing and promote: the **`ship-it` skill**.

- **Build ≠ deploy.** Merges build `ghcr.io/gjovanov/roomler-ai:hosted-<YYYYMMDD>-<sha7>`.
  Nothing rolls until `gh workflow run promote.yml -f tag=…`; ArgoCD then rolls it in
  ~5 s. The lane never writes `latest`, which is the self-host `full` image.
- **Pods**: namespace `roomler-ai`, deployment **`roomler2`**, 2 replicas, hostNetwork,
  maxSurge 0.
- ⚠️ **"Video works for org A but not org B" is a per-NODE media path**, not an app
  bug: the host lacks its RTC-range (40000–49999) DNAT/SNAT/TTL rules. They come from
  the Ansible host vars, so never hand-fix a host. Tell:
  `media transport has no DTLS 15s after connect_transport`.
- ⚠️ **Tenant affinity pins long-lived sockets to one pod.** After an upstream flip,
  also `kubectl rollout restart deploy/roomler2`. A half-open control WS behind a
  TLS-inspecting middlebox shows GREEN but is `agent_offline`.
- **After every roll, field-verify from the fleet**: pods on the new image, the
  online-agent count, an RC session, an overlay pair, a tunnel forward. An exec sweep
  is a biased sample, so take the denominator from the server.

## Functional Requirements (FR) workflow — STANDING RULE (operator, 2026-08-27)

Every substantial feature or multi-step program — a new capability, a performance
arc, a protocol change, anything bigger than a one-PR fix — gets a **Functional
Requirement** tracked in GitHub. **Creating the FR is a step of the PLAN**, not a
write-up filed afterwards. Registry and protocol: **`docs/fr/README.md`**; the
measurements behind each step: `.claude/rules/fr-workflow.md`.

0. **Open the issue BEFORE you implement.** It is the only collision guard that works
   across parallel sessions. Re-fetch master and list open FR issues first.
1. **Spec** `docs/fr/FR-N-<slug>.md`: goal, evidence, design with `file:line` anchors
   verified against master, phases with kill switches, acceptance criteria as
   checkboxes, open decisions, out-of-scope, field log. ⚠️ **Claim N by adding your row
   to `docs/fr/README.md` in the same commit**, never by scanning the directory. On a
   collision the **lower issue number keeps FR-N**.
2. **Issue** `FR-N: <title>` linking the spec (Goal / Key design / Acceptance criteria /
   Open decisions / Out of scope / Related).
3. **Comment as you ship**: a `## Step log` row per merged PR, then
   `## Result — field-verified on <version>`. ⚠️ **CI green is not a result.** Show the
   field test FAILING on the current deploy first.
4. **Close** only when the acceptance criteria are field-verified, and **tick the boxes
   in the same breath** (`scripts/fr-verification-debt.sh` checks daily). A regression
   reopens it.
5. **Docs before close**: the docs for what it built, in house style (mermaid, tables,
   `file:line` anchors, a `docs/README.md` row). Every spec carries this as a phase row
   and an acceptance criterion.

## Post-Implementation Testing

Run the **most specific** command first; if a backend change also affects the
frontend, run both.

| Change type | Command |
|---|---|
| Backend (models, services, routes) | `cargo test -p roomler-ai-tests` |
| Remote-control crate (signalling, wire format) | `cargo test -p roomler-ai-remote-control --lib` |
| Agent library | `cargo test -p roomlerd --lib` |
| Agent with media / input backends | `cargo test -p roomlerd --lib --features full` |
| Agent capture, headless | `./scripts/dev-xvfb.sh` |
| Frontend | `cd ui && bun run build` |
| Frontend unit | `cd ui && bun run test:unit` |
| Full flow | `cd ui && bun run e2e` |

⚠️ The agent builds and unit-tests **natively on Windows**
(`cargo test -p roomlerd --lib`) — the WSL lane is not the only option.

## Pillar invariants — `.claude/rules/`

Each pillar's invariants are a **path-scoped rule** that loads automatically when you
read or edit a file its `paths:` match. **Read the matching rule before designing a
change**; a plan made from the docs alone has missed these before.

| Rule | Loads with | Covers |
|---|---|---|
| `networking.md` | agent, tunnel/DERP crates, network + fleet modules | pillar 2's six commitments, leases, exit nodes, multi-org, peer relays, SSH, fleet RPC, remote config |
| `remote-desktop.md` | agent, companion, remote_control, remote module, RC UI | the `rc:*` wire, `RpcCap` verbs, encoders, consent |
| `modular-monolith.md` | core, modules, api, composition baseline | the module DAG, hooks, composition gate, profiles, `State<Core>` |
| `security.md` | server + UI code | the baseline below, with the reasoning |
| `testing.md` | `crates/tests`, `ui/e2e` | the lane's assertions and skips, the E2E rig |
| `fr-workflow.md` | `docs/fr/` | the FR protocol with its measurements |
| `installers.md` | wizard, packaging | installer proxies, UAC, retired wizards |
| `deployment.md` | promote/image workflows, conference module | pods, media-path DNAT, tenant affinity |

These bind with no code open, fleet debugging included:

- ⚠️ **A WebRTC peer MUST be `close()`d. Dropping it frees NOTHING.** Its UDP sockets
  are owned by tasks the ICE agent spawned, so an `Arc` drop leaves every one of them
  live. This once consumed the entire ephemeral port range and **took host DNS down**
  while `ping 1.1.1.1` stayed at 3 ms. That signature (names unresolvable, IPs fine)
  is socket exhaustion, **not** a DNS-server problem (`docs/tunnels.md`).
- ⚠️ **`roomler exec` re-splits argv.** A Windows target needs ONE quoted argument,
  and `;` never `&&`. On a host that restarts its own service it answers "no answer
  within 45 s", but the command ran.
- ⚠️ **Linux root daemons resolve `/etc/roomler/config.toml`**, and `systemctl
  is-active` reads *inactive* while such a host is perfectly healthy (the live daemon
  is an unmanaged orphan). Check `pgrep -x roomlerd` and `roomler peers`, and never
  "restart to fix" on that basis.

## Installation and packaging

`docs/installation.md` · `docs/code-signing.md` · the **`ship-it` skill**, with detail
in `.claude/rules/installers.md`. ONE unified wizard (`agents/roomler-setup`, Tauri 2)
covers every role. Installer bytes stream through `roomler.ai` (`/api/setup/*`,
`/api/agent/installer/*`), **not** `github.com`, so corporate ESET/Defender
allow-lists trust the download.
⚠️ **UAC naming rule**: Windows UAC auto-elevates any EXE whose filename contains
"install", "setup", "update" or "patch", and cargo names test binaries after the
**lib** crate. So a lib target must dodge those substrings (`wizard_app`,
`wizard_shared`), and a user-facing bin that keeps one sets `[[bin]] test = false`.

## Known Issues (OPEN only)

Fixed-and-shipped issues live in `git log` and the docs.

- **[MEDIUM]** [2026-08-23] **User sessions are irrevocable**: no `token_epoch`, no
  password-change route, no refresh rotation (`docs/security-baseline.md` §1).
- **[MEDIUM]** [2026-08-23] **The updater's manifest is unsigned**: version + url +
  hash are not attested as a unit. Artifacts are publisher-verified on all three OSes,
  and since FR-97 P6a they carry their version inside the signed bytes
  (`docs/code-signing.md` §7b). A hostile manifest can therefore withhold an update
  but not downgrade one. The tunnel CLI has been key-pinned since `tunnel-v0.4.149`.
- **[MEDIUM]** [2026-04-17, FR-27] **Host consent**: every `PromptSurface` is
  field-verified except the **macOS native panel**. On Wayland the native panel is X11
  through Xwayland, with no stacking guarantee (#2286). Invariants:
  `.claude/rules/remote-desktop.md`.
- **[LOW]** [2026-08-03] Overlay ACL is not field-proven under `enforce`. Flip a
  tenant to `warn` and read `rx_denied` first.
- **[LOW]** [2026-04-20] NVENC `ActivateObject` fails with 0x8000FFFF on RTX 5090
  (Blackwell). The cascade routes around it and the probe filters AV1 from caps.
  Re-test on newer drivers.
- **[LOW]** [2026-03-10] No lint git hooks and no secret scanner (gitleaks/trufflehog)
  in CI.

## Security Baseline

The rules that bind new code. The reasoning behind each is in
**[`docs/security-baseline.md`](docs/security-baseline.md)** and in
`.claude/rules/security.md`, which loads with server or UI code. Permission bits:
`docs/permissions.md`.

- **`is_member(tid)` is NOT authorization for anything keyed by id.** Resolve the
  object *within* the tenant (`helpers::require_room_in_tenant` /
  `require_message_in_tenant`; a `message_id` handler uses the message guard), so a
  foreign id 404s.
- **A 403 is an answer, not an expired credential.** It never ends a session. The only
  403 that navigates is `not_a_member`, matched on a server-sent code.
- **Managed roles reconcile additively** (`stored | definition`). ⚠️⚠️
  `EXEC_DEVICE`/`SSH_DEVICE` stay out of every row below `ADMINISTRATOR`.
- **`users.email` holds an address only if that account PROVED it.** It is a unique
  reservation that account-linking keys off; Microsoft is always unverified.
- **Agent tokens are status-checked on every use** (`AuthAgent`). A lookup failure is
  **500, not 401**, and deletion wins over status.
- **A WG public key can't be claimed by two live nodes** in a network
  (`wg_key_taken_by_other`, fail-closed).
- **Push endpoints are SSRF-validated** at subscribe time. **Uploads are sniffed**,
  never trusted. The **message-HTML allowlist** (`useMarkdown.ts`) is the only XSS
  boundary for message content.
- **The CSP allows `http://127.0.0.1:*` on purpose** for the RC viewer's loopback
  probes. Exercise the remote-control page when touching CSP.
- **Publishing identity is an allowlist.** The repo is public, so an author email or a
  GitHub login leaks *who*. `gh auth switch` is global; use `scripts/gh-scoped-config.sh`.
- Prod refuses the default JWT secret, cookies are `Secure`, CORS defaults to the
  frontend origin, and requests are capped at 60/min per IP, with per-(caller, device)
  exec/SSH ceilings after the identity gates.
