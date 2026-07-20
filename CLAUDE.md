# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**Roomler AI** is a real-time collaboration platform with chat, video conferencing, file sharing, room management, and a TeamViewer-style remote desktop subsystem. Stack: Rust (Axum) + MongoDB + Vue 3/Vuetify 3 + Pinia + Mediasoup (WebRTC SFU) + webrtc-rs (P2P remote-control). The remote-control subsystem ships as a separate native agent binary (`roomler-agent`) that runs on controlled hosts — see `docs/remote-control.md`.

## Commands

```bash
# Development
cargo run --bin roomler-ai-api         # Start backend (port 3000)
cd ui && bun run dev                   # Vite dev server (port 5000, proxies to 5001)
cd ui && bun run build                 # Production UI build (includes vue-tsc --noEmit)

# Remote-control agent (native binary — runs on the controlled host)
cargo build -p roomler-agent --release --features full      # full pipeline: capture + encode + input (SW encoder)
cargo build -p roomler-agent --release --features full-hw   # Windows + Media Foundation HW encoder scaffolding (opt-in)
cargo build -p roomler-agent --release                      # signalling-only (no media, no input)
./target/release/roomler-agent enroll --server <url> --token <enrollment-jwt> --name <label>
./target/release/roomler-agent run
./target/release/roomler-agent run --encoder software       # force openh264 (default on Windows today)
./target/release/roomler-agent run --encoder hardware       # try MF-HW → MF-SW → openh264 (experimental)
./target/release/roomler-agent encoder-smoke --encoder hardware   # offline: feed 10 synthetic frames, diagnose MFT init
./scripts/dev-xvfb.sh                  # capture smoke test via a virtual framebuffer

# Testing
cargo test -p roomler-ai-tests           # All integration tests (163+ tests, requires MongoDB+Redis)
cd ui && bun run test:unit             # Vitest unit tests (259 tests)
cd ui && bun run test:unit:coverage    # Vitest with coverage
cd ui && bun run e2e                   # Playwright E2E tests (24 spec files)

# Static Analysis
cargo fmt --all -- --check                  # Rust fmt (matches CI)
cargo clippy --workspace --all-targets --all-features -- -D warnings   # Rust lint (matches CI — include --all-targets so test-only lints fire)
cargo check --workspace                     # Rust compilation check
cd ui && vue-tsc --noEmit                  # Vue TypeScript check

# Dependency Audit
cargo audit                            # Rust CVE scan (requires cargo-audit)
cargo outdated                         # Rust outdated deps (requires cargo-outdated)
cd ui && bun audit                     # JS/TS vulnerability scan
cd ui && bun outdated                  # JS/TS outdated deps

# Infrastructure
docker compose up -d                   # Start MongoDB (27019), Redis (6379), MinIO (9000), coturn
```

### Agent build requirements

`--features full` (or the individual `scrap-capture` / `openh264-encoder` / `enigo-input` flags) pulls in system deps:

```bash
# Linux (for the scrap-capture feature)
sudo apt install -y libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev

# OpenH264 is compiled from C source on first build — slow but no runtime lib needed.
```

Default build (no features) compiles on any rust:bookworm image and produces a signalling-only agent useful for CI / integration tests, but not usable in production (no capture, no input).

### Encoder selection (Windows)

Preference resolution: **CLI `--encoder` > env `ROOMLER_AGENT_ENCODER` > `encoder_preference` in config TOML > `Auto` default**. Values: `auto` | `hardware` (`hw`/`mf`) | `software` (`sw`/`openh264`). `Auto` on Windows runs the MF H.264 probe-and-rollback cascade then falls back to openh264; everywhere else it's openh264 only. Escape hatch `ROOMLER_AGENT_HW_AUTO=0` reverts to openh264-first without a rebuild. Full cascade mechanics (DXGI adapter enumeration, async-unlock semantics, probe frame, Intel QSV async path) in `docs/remote-control.md`.

## Architecture

```
crates/
  config/           → Settings (env vars via ROOMLER__ prefix, config crate)
  db/               → MongoDB models (19 models) + indexes (18 collections) + native driver v3.2
  services/         → Business logic: auth, DAOs, media (mediasoup), export, background tasks, OAuth, push, email, Stripe, Giphy, Claude AI
  remote_control/   → TeamViewer-style remote-desktop subsystem: Hub, signalling, consent, audit, TURN creds
  api/              → Axum HTTP/WS server: ~85 API routes + /ws + /health
  tests/            → Integration tests (24 test modules, 163+ tests)
agents/
  roomler-agent/    → Native remote-control agent binary (CLI + lib): webrtc-rs peer, scrap capture, openh264 encode, enigo input injection
ui/
  src/
    api/            → HTTP client (client.ts)
    components/     → Vue components (20+ files in 7 categories — includes admin/AgentsSection)
    composables/    → 11 custom hooks (useAuth, useWebSocket, useMarkdown, useRemoteControl, etc.)
    stores/         → 13 Pinia stores (setup store pattern — includes agents.ts)
    views/          → 14 view modules (auth, chat, conference, dashboard, files, rooms, remote, etc.)
    plugins/        → router, pinia, vuetify, i18n
scripts/
  dev-xvfb.sh       → Run the agent's capture path against a virtual X framebuffer (headless smoke test)
```

### Crate dependency flow
`config` <- `db` <- `remote_control` <- `services` <- `api`
`tests` depends on `api` + `config` + `db` + `roomler-agent` (spawns real servers with random ports and test databases; drives the agent library in-process for end-to-end signalling tests)

## Multi-Tenancy

All data is scoped by `tenant_id`. Routes are nested: `/api/tenant/{tenant_id}/room/{room_id}/message/...`. The `tenant_members` collection tracks user-tenant membership. Room membership is tracked via `room_members`.

## Auth Pattern

JWT-based auth (jsonwebtoken 9 crate) with Argon2 password hashing:
- Access token: configurable TTL (default 604800s = 7 days)
- Refresh token: configurable TTL (default 2592000s = 30 days)
- Auth middleware extracts user from `Authorization: Bearer` header
- OAuth: Google, Facebook, GitHub, LinkedIn, Microsoft

Four `TokenType` variants, all signed with the same JWT secret:
- `Access` / `Refresh` — standard user flow
- `Enrollment` — single-use, 10 min, issued by an admin to bootstrap a new agent
- `Agent` — long-lived (1 y), carried by an enrolled agent on its WS connection

Audience checks: `verify_agent_token` rejects a user JWT and vice-versa. Tests in `crates/services/src/auth/mod.rs::tests` lock this.

JWT settings in `crates/config/src/settings.rs`:
- Secret: `ROOMLER__JWT__SECRET` (default: "change-me-in-production")
- Issuer: `ROOMLER__JWT__ISSUER` (default: "roomler-ai")

## Route Pattern

```rust
// Axum nested routers under /api/tenant/{tenant_id}/...
let room_routes = Router::new()
    .route("/", get(routes::room::list))
    .route("/", post(routes::room::create))
    .route("/{room_id}", get(routes::room::get))
    .route("/{room_id}", put(routes::room::update))
    .route("/{room_id}", delete(routes::room::delete));

// Composed in build_router():
Router::new()
    .nest("/api/tenant/{tenant_id}/room", room_routes)
    .with_state(state)
```

Route groups: auth (7), user (2), oauth (2), stripe (4), invite (2+4), giphy (2), push (3), notification (5), tenant (3), member (2), role (6), room (16), message (11), recording (3), file (7), task (3), export (2), search (1), health (1), ws (1), agent (4 tenant-scoped + 1 public enroll), session (3), turn (1).

## DB Model Pattern

MongoDB native driver (not Mongoose). Models live in `crates/db/src/models/` except the three remote-control entities, which live in `crates/remote_control/src/models.rs` to keep the subsystem self-contained:
- 18 collections: tenants, users, tenant_members, roles, rooms, room_members, messages, reactions, recordings, files, invites, background_tasks, audit_logs, notifications, custom_emojis, activation_codes, **agents, remote_sessions, remote_audit**
- Indexes defined in `crates/db/src/indexes.rs` (unique, TTL, text indexes on email, username, slug, code, content, etc.)
- Text indexes on messages (content), rooms (name, purpose, tags), users (display_name, username) for full-text search
- TTL indexes on audit_logs (90 days), activation_codes, background_tasks, **remote_audit (90 days)**
- Unique composite index on `agents.{tenant_id, machine_id}` so re-enrolling a known machine reuses its row
- All queries use BSON documents, no ORM

## Frontend Conventions

- **Plugin order**: i18n -> vuetify -> pinia -> router (in main.ts)
- **Vuetify**: Light + dark themes, auto-import tree-shaking via `vite-plugin-vuetify`
- **Stores**: Pinia with setup store pattern (`defineStore('name', () => { ... })`)
- **Rich text**: TipTap v3 with markdown support, mentions, emoji
- **WebRTC**: Mediasoup client for video conferencing
- **API client**: `ui/src/api/client.ts` with auth token injection
- **Vite proxy**: `/api` and `/ws` proxied to `http://localhost:5001`
- **Responsive page padding**: top-level views use `<v-container fluid class="pa-2 pa-md-4 pa-xl-6">` (8px mobile / 16px tablet+ / 24px ≥1920px). Empty-state blocks use `pa-4 pa-md-6 pa-lg-8`. Headings use `text-h5 text-md-h4` so they shrink one step on phone. Section CTAs use `size="large"` (not `x-large`) — the wider button overflows narrow viewports. Marketing/legal sections (`LandingView`, `Terms`, `Privacy`) replace fixed `py-12`/`py-16` with `py-6 py-md-12` / `py-8 py-md-16`. Custom-flex views (`ChatView`, `ConferenceView`) own their own layout and intentionally don't use `<v-container>`. Hide secondary toolbar items on `<sm` with `d-none d-sm-inline-flex`; surface a phone fallback alongside.

## Test Setup

**Integration tests** (`crates/tests/`):
- Each test gets a unique UUID-named database, auto-dropped on teardown
- Tests spawn real Axum servers on random ports
- Requires MongoDB on `localhost:27019` and Redis on `localhost:6379`
- 163+ tests across 24 modules: auth, tenant, room, message, reaction, recording, file, invite, role, notification, push, giphy, oauth, call, pagination, rate_limit, cors, billing, multi_tenancy, channel_crud, pdf_export, conference_message, **remote_control, agent** (full rc:* round-trip drives the agent library in-process against a TestApp)
- 5 known pre-existing failures (CORS tower-http upgrade, role dedup, rate-limit timing) — reproducible on pristine master and unrelated to recent work

**E2E tests** (`ui/e2e/`):
- Playwright 1.58 with Chromium (fake media stream devices for WebRTC)
- 24 spec files: auth, channels, chat, chat-multi, chat-pagination, chat-reactions, chat-threads, conference (4 specs), connection-status, dashboard, files, invite, mention, notifications, oauth, profile, room-fixes, room-management, websocket, 404
- Fixtures in `ui/e2e/fixtures/test-helpers.ts`
- Base URL: `http://localhost:5000` (or E2E_BASE_URL env var)

**Unit tests** (`ui/src/`):
- Vitest with jsdom environment, 259 tests across 16 files
- Stores: auth, messages, rooms, ws (incl. rc:* channel), notifications, conference, tenants, files, agents
- Composables: useValidation, useSnackbar, useMarkdown, useRemoteControl (HID + button mapping locks)
- API client: token injection, error handling
- Plugins: vuetify theme config

**Rust unit tests** (in-crate `#[cfg(test)] mod tests`):
- `remote_control` crate: 20 tests (consent, session state machine, signalling, serde wire-format locks, permissions, TURN creds)
- `roomler-agent` lib (default features): 5 tests; plus 4 openh264, 3 enigo, 1 scrap under the matching feature flags
- `services::auth`: 5 tests (token roundtrip + cross-audience rejection)

**Capture smoke test** (no desktop required):
- `./scripts/dev-xvfb.sh` spins up Xvfb, paints an xterm on it, runs the scrap-capture smoke test against that virtual display. See docs in the script header for subcommands (`run`, `shell`, arbitrary pass-through).

## Environment

- `.env` — development (not committed, in .gitignore)
- Config via `ROOMLER__` prefixed env vars (double underscore separator)
- Docker: `docker-compose.yml` runs MongoDB 7 (auth credentials defined in `docker-compose.yml`; local dev only), Redis 7, MinIO, coturn
- Default DB URL: `mongodb://localhost:27019` (tests use no auth)

## Deployment

- **Production URL**: `https://roomler.ai/` — the live deployment. Use this as the `--server` argument when enrolling agents and as the origin the browser controller loads.
- **Docker**: Multi-stage build (rust:1.88-bookworm -> oven/bun:1 -> debian:trixie-slim + nginx)
- **Deploy repo**: `<deploy-repo>` on the build host. Kustomize manifests live under `k8s/base/` + `k8s/overlays/prod/`. Ansible playbooks retained for host-level tasks only (HAProxy, WireGuard, iptables).
- **GitOps**: ArgoCD (at `<argocd-host>`) reconciles the `roomler-ai` Application from the deploy repo's `master` branch, path `k8s/overlays/prod`. Sync policy is **Automated + selfHeal + prune** with a GitHub webhook on the deploy repo: `git push` to master rolls out within ~5 s. 60 s polling fallback via `argocd-cm.timeout.reconciliation: 60s`. Sibling Application CRDs (bauleiter / lgr / oxmux / purestat / regal / roomler-ai / roomler-old / tickytack) are gitops-managed under a parent app-of-apps. Verify the live targetRevision with `argocd app get roomler-ai --grpc-web | grep -E "Target|Sync Status"`.
- **Image registry**: `<internal-registry>` (self-hosted Docker Registry v2 on the build host, basic auth, cert auto-renewed via acme.sh). Pull secret `regcred` lives in the `roomler-ai` namespace.
- **K8s cluster**: 3 control-plane + 3 worker nodes (Ubuntu 22.04, containerd 1.7.29, v1.31.14). Three zones via `topology.kubernetes.io/zone` (one master + one worker VM per bare-metal host).
- **Tier policy** (added 2026-05-01): cluster nodes are labelled `tier=high-performance` (the two high-perf worker hosts) and `tier=utility` (the build/utility worker host). roomler-ai schedules on `tier=high-performance` only — never on the utility worker. Enforced via a Kustomize patch in `<deploy-repo>/k8s/overlays/prod/kustomization.yaml` (commit `dab3cfa`) that adds a required `nodeAffinity` to every Deployment + StatefulSet. Hostname pin in `base/` (`kubernetes.io/hostname: <storage-pinned-worker>`) is intentionally retained — the StatefulSet PVCs use node-local storage, so the data lives on that specific node; the tier requirement is an *additional* constraint, both must match. **Utility worker hosts**: monitoring (kube-prometheus), `<internal-registry>`, image builds (direct on the host), `bauleiter`, `regal`. **High-perf workers**: roomler (old), roomler-ai, oxmux, clawui (when migrated to K8s), lgr, purestat, tickytack.
- **Pod placement**: roomler-ai's pods run on `<storage-pinned-worker>` (`<worker-node-ip>`). Namespace `roomler-ai`, deployment `roomler2` (note: name is `roomler2` not `roomler-ai`), Recreate strategy, hostNetwork, `imagePullPolicy: IfNotPresent`.
- **Health probes**: startup/readiness/liveness all on `/health` (port 80 via nginx -> :3000 backend)
- **nginx**: Pod-internal reverse proxy (`files/nginx-pod.conf`) — SPA fallback + API proxy + WS proxy
- **Agent binary**: built separately (`cargo build -p roomler-agent --release --features full`) and distributed to controlled hosts via GitHub Releases (MSI / .pkg / .deb auto-built by `.github/workflows/release-agent.yml` on `agent-v*` tag push). Not part of the API Docker image.

### K8s deploy pipeline (ArgoCD GitOps)

The build host builds the image, pushes to `<internal-registry>/roomler-ai:<tag>`, bumps the tag in the gitops repo, and ArgoCD reconciles the Deployment. Fill in the env vars at the top once per shell session:

```bash
# Operator-filled (set once per shell):
: "${BUILD_HOST:=ssh-target}"            # e.g. your build host alias
: "${REGISTRY:=registry.example.com}"    # your <internal-registry>
: "${REPO:=$HOME/roomler-ai}"            # local clone of this repo on the build host
: "${DEPLOY_REPO:=$HOME/roomler-ai-deploy}"

ssh "$BUILD_HOST"
cd "$REPO" && git pull
docker build -t "$REGISTRY/roomler-ai:build-$$" .                   # ~5–15 min (cache warm)
TAG="v$(date +%Y%m%d)-$(docker images -q "$REGISTRY/roomler-ai:build-$$" | head -c 12)"
docker tag "$REGISTRY/roomler-ai:build-$$" "$REGISTRY/roomler-ai:$TAG"
docker tag "$REGISTRY/roomler-ai:build-$$" "$REGISTRY/roomler-ai:latest"
docker push "$REGISTRY/roomler-ai:$TAG"
docker push "$REGISTRY/roomler-ai:latest"

# ── ALWAYS run after every deploy: reclaim the build's disk footprint. ──
# The image is safely in the registry now, so the local copies are just build
# leftovers. Every deploy bakes a fresh multi-stage image (+ intermediate layers
# + build cache); without pruning they pile up until the build host's root FS
# fills. (2026-07-12: `/` hit 100% from ~13 GB of stale build images mid-deploy.)
# `-a` drops images not backed by a RUNNING container, so the mongo + registry
# containers (and their images) are untouched; NO `--volumes`, so mongo DATA is
# safe. Reclaims the per-deploy delta every time.
docker system prune -af
docker builder prune -f
df -h / | awk 'NR==2{print "build-host / : "$4" free ("$5")"}'   # sanity

cd "$DEPLOY_REPO"
git checkout master && git pull
sed -i "s|newTag:.*|newTag: $TAG|" k8s/overlays/prod/kustomization.yaml
git commit -am "chore(k8s): bump roomler-ai to $TAG"
git push

argocd app sync roomler-ai --grpc-web     # or Sync via the ArgoCD UI
curl -sI https://roomler.ai/health        # HTTP/2 200
```

Registry retention: `registry-retention.sh 1` (weekly cron at Sun 04:00) keeps at most 2 tags per repo (latest + most-recent-versioned) and GC's the registry storage. **Run it manually if `/gjovanov/registry` is fat** — the blob store isn't touched by `docker system prune` (it's the registry's own storage, not docker's), and heavy repos (e.g. `lgr` at ~7.5 GB/image) accrete fast between weekly GCs.

**Periodic build-host maintenance (NOT per-deploy):** the fattest reclaimables are the local Rust `target/` dirs of the *other* projects cloned on the build host (`~/{harvex,oxmux,purestat,parakeet-rs}/target` were ~44 GB combined on 2026-07-12) — `cargo clean` or `rm -rf <proj>/target` when idle; they just recompile on next build. **Never touch `/var/lib/libvirt`** (the running k8s master+worker VM disks, ~87 GB) or the active container data volumes.

## Post-Implementation Testing

After every feature or fix, verify your changes:

| Change type | Command | What it checks |
|-------------|---------|----------------|
| Backend (models, services, routes) | `cargo test -p roomler-ai-tests` | Integration tests (real MongoDB) |
| Remote-control crate (Hub, signalling, wire format) | `cargo test -p roomler-ai-remote-control --lib` | Unit tests (no MongoDB required) |
| Agent library | `cargo test -p roomler-agent --lib` | Default-feature unit tests |
| Agent with media / input backends | `cargo test -p roomler-agent --lib --features full` | Needs libxcb*-dev on Linux |
| Agent capture against a headless display | `./scripts/dev-xvfb.sh` | Xvfb + xterm + capture smoke test |
| Frontend (views, stores, composables) | `cd ui && bun run build` | TypeScript + Vite build |
| Frontend unit tests | `cd ui && bun run test:unit` | Vitest (259 tests) |
| Full-flow (auth, routes, UI+API) | `cd ui && bun run e2e` | Playwright E2E tests |

Run the **most specific** command first. If a backend change also affects the frontend, run both.

### Defensive enum catch-alls

`ClientMsg` / `ServerMsg` in `crates/remote_control/src/signaling.rs` are matched exhaustively from multiple consumer crates (agent, api/ws, hub). When you preemptively add a `_ =>` / `other =>` catch-all arm in a consumer match **without** adding the new variants that would make it reachable in the same commit, `cargo clippy --workspace -- -D warnings` fails with `unreachable_patterns` (the existing arms already cover every known variant). CI run [25972574628](https://github.com/gjovanov/roomler-ai/actions/runs/25972574628) hit this — defensive catch-all landed in `ec61f03` before the corresponding T2 wire variants did. The rule:

- If the new variants are landing in **the same commit**: no allow needed, the catch-all is immediately reachable.
- If the new variants are landing in **a later commit** (defensive future-proofing): annotate the catch-all with `#[allow(unreachable_patterns)]` and reference this rule in a comment so the next reviewer doesn't strip the allow. Remove the allow when the variants land.
- `#[non_exhaustive]` on the enum upstream is the structural alternative but forces a catch-all in every consumer everywhere — too invasive for the existing `signaling::*` matches.

## Remote Control Subsystem

TeamViewer-style remote desktop. One native agent per controlled host, Roomler API as signalling-only relay, browser as controller. All media + input flows over direct WebRTC P2P (TURN-relayed if needed) — the server never sees raw pixels or keystrokes.

**Design + architecture**: `docs/remote-control.md` (16 sections covering goals, topology, protocol, data model, security, latency budget). Overlay-mesh sub-topics: `docs/agent-tunnel-architecture.md`, `docs/overlay-wfp.md` (Windows firewall override), **`docs/overlay-exit-nodes.md`** (Tailscale-style exit nodes), **`docs/overlay-nat-traversal.md`** (carrier cascade: LAN→direct-public→srflx hole-punch→relay).

**Overlay exit nodes** (`overlay-l3`, default-OFF): a client routes its whole internet egress (`0.0.0.0/0` + `::/0`) through a chosen mesh peer. Config keys: exit offers with `overlay_exit_node_enabled=true`; an admin approves via `PUT …/overlay-node/{id}/exit-node` (writes `is_exit_node` + adds `/0` to `approved_routes` — the data-plane signal, NOT `is_exit_node` alone); a client opts in with `overlay_exit_node="<name|hex>"`. Core invariant = **never self-wedge**: pin `/32`/`/128` carrier+control exemptions first, then install the split-default (`0.0.0.0/1`+`128.0.0.0/1`; v6 `::/1`+`8000::/1`), else WITHHOLD; route-guard re-asserts every 2 s; boot-reconciler + `purge_exit_routes()` heal a stale `/1` after a hard exit. DNS steered to the exit's vantage (no leak). ⚠️ An exit reroutes the host's own *inbound*-reply traffic → it breaks un-exempted SSH; and NEVER run the exit field-test on a prod cluster node (see `docs/overlay-exit-nodes.md` caveats). Full detail in `docs/overlay-exit-nodes.md`.

**Resumption note after a session break**: see `git log` and `docs/remote-control.md` — per-release handover files were retired 2026-05-23 as part of a privacy/security cleanup.

**Wire protocol**: `rc:*` JSON messages over the existing `/ws` endpoint. `ClientMsg` / `ServerMsg` in `crates/remote_control/src/signaling.rs`. ObjectIds are raw hex strings (locked by tests); `Permissions` serialises as pipe-separated names (bitflags 2.x convention, also locked).

**WebSocket role multiplexing**: `/ws?token=<jwt>&role=agent` uses the agent JWT audience; no `role` param (or `role=user`) uses the existing user flow. Same WS endpoint, same handshake, different claim validator.

**Unified installer (P4, 2026-07-17)** — ONE wizard for the whole node stack:
- **`agents/roomler-setup/`** (Tauri 2 single-window app, lib `wizard_app`) + **`crates/roomler-setup-core/`** (event-shape-free mechanics, lib `wizard_shared`). Role picker on Welcome: three daemon flavours on Windows (perMachine-SCM service / perUser task / perMachine attended — mapping to the MSI flavours) + tunnel-client on any OS. Steps: Welcome/role → Server → Token → Install → Done, with cancel/force-kill, progress replay, cross-flavour ack gate, and wizard-state persistence (**token NEVER persisted**). Daemon roles also place the `roomler-desktop` companion EXE (GAP-A). Released by `release-setup.yml` on `setup-v*` tags (Linux/macOS tarballs + SIGNED Windows EXE in `.zip`); first field-proven at setup-v0.3.0-rc.197.
- **Backend proxies** in `crates/api`: `/api/setup/{latest-release,{platform}/health,{platform}}` serves the wizard itself (routes/setup_release.rs) + `/api/setup/install.{sh,ps1}` serve the terminal (no-GUI) installers embedded at compile time from `scripts/`. `/api/agent/installer/{flavour}` + `/health` (routes/agent_release.rs) streams MSI bytes through `roomler.ai` (NOT `github.com`) so corporate ESET / Defender allow-lists trust the download; `/api/tunnel/installer/{platform}` serves the CLI tarball.
- **UAC lib-naming rule** — Windows UAC's "installer detection" heuristic auto-elevates any EXE whose filename contains "install" / "setup" / "update" / "patch"; cargo derives test-binary names from the LIB crate, so wizard lib targets must dodge those substrings (`wizard_app`, `wizard_shared`; historically `wizard_core` / `tunnel_wizard_core`). The user-facing bin EXE keeps the marketing name; `[[bin]] test = false` keeps `cargo test -p roomler-setup` off the UAC prompt.
- **Legacy wizards RETIRED in P4c-2** — `agents/roomler-installer` (rc.28 agent wizard) and `agents/roomler-tunnel-installer` (rc.59 tunnel wizard) were reduced to shims over `wizard_shared` in P4a and deleted after the unified wizard's field-proof, along with `release-tunnel-wizard.yml`, the installer-EXE half of release-agent.yml's companions job, and the legacy `/api/tunnel-wizard/*` route family. The tunnel CLI's `self-update` is KEPT — it's the sole updater for tunnel-only hosts ("one updater" is per-role; daemon hosts get `roomler.exe` refreshed by the MSI).

**Declared tunnel routes (P6)** — `roomlerd` supervises forwards/SOCKS5 listeners declared in its config (`[[tunnel_routes]]`, `tunnel_core::localapi::RouteDescriptor` = one type for wire + disk): `agents/roomler-agent/src/tunnel/route_reconciler.rs` reconciles them into hub flows on every start (create-retry backoff; terminal `failed` on revoked/cross-tenant so a dead route never hammers the server), `roomler route add/rm/ls/enable/disable` + the desktop Tunnels section manage them over the LocalAPI `Route*` verbs, and the daemon persists through an atomic `config::save` + a daemon-wide write lock. See `docs/tunnel-install.md` §6 "Declared routes".

**Older releases (0.1.x → 0.3.0-rc.26)**: CLAUDE.md no longer mirrors per-release notes — `git log` and `docs/remote-control.md` are authoritative for the historical arc. Key milestones, all shipped: live-verified WebRTC P2P (0.1.36), MF H.264 HW cascade with REMB-driven bitrate (0.1.26), codec negotiation H.264/HEVC/AV1 with probe-at-startup filter (0.1.28-0.1.30), clipboard + file-transfer data channels (0.1.32-0.1.33), WebCodecs canvas render bypass (0.1.36, Tier B7), agent lifecycle service hooks + auto-update (0.1.36), failure-resilience cycle with watchdog + crash rollback + SHA256 verification (0.1.50-0.1.54), heartbeat telemetry + pre-flight checks + opt-in Windows Service mode (0.1.55-0.1.58), M5 verification + clean-exit fixes + install-storm cooldown (0.1.61-0.1.63), M3 Z-path lock-screen overlay + browser auto-reconnect + perMachine MSI (0.2.0-0.2.5; A1 WGC NO-GO empirically confirmed), auto-update asset-picker flavour-aware (0.2.6), input regression fix (0.2.7), M3 A1 SystemContext-from-cold-start (rc.1-rc.7), UAC self-update + cross-flavour MSI cleanup + Tauri tray companion (rc.18), resumable file-DC transfers (rc.19-rc.20), ESET-evasive PROGRAMDATA staging (rc.21-rc.22), SystemContext Winlogon + elevated apps gating via `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP` (rc.26).

## Known Issues (OPEN only)

Fixed-and-shipped issues live in `git log`. Currently open:

- [HIGH] [2026-03-10] JWT default secret is "change-me-in-production" — must be overridden in prod. A startup `tracing::error!` fires if the default is in use (2026-05-23 cleanup); a hard-fail gated on an explicit `app.environment=production` setting is the next step.
- [MEDIUM] [2026-04-17] Remote-control: consent auto-granted on agent (no tray-driven prompt yet); fine for self-controlled hosts, needs UI for org-controlled devices per docs §11.2.
- [MEDIUM] [2026-05-23] CORS falls back to `Any` origin when `cors_origins` is unset or contains `"*"` (`crates/api/src/lib.rs:22-36`) — operator-misconfig risk; tighten the default in a follow-up.
- [MEDIUM] [2026-05-23] File upload trusts the client-supplied `content-type` header (`crates/api/src/routes/file.rs:226-231`); no MIME whitelist. MIME-confusion risk when files are later served back.
- [LOW] [2026-05-23] Agent `config.toml` holds `agent_token`. Unix saves with `0600`; Windows currently relies on the default user ACL — `agents/roomler-agent/src/config.rs` should set an explicit ACL.
- [LOW] [2026-05-23] nginx (`files/nginx-pod.conf`) is missing `Strict-Transport-Security` and `Content-Security-Policy` headers.
- [LOW] [2026-03-10] Deployment strategy is Recreate (no zero-downtime rolling updates).
- [LOW] [2026-03-10] No git hooks configured (no pre-commit, no lint-staged).
- [LOW] [2026-04-20] Remote-control: NVIDIA NVENC `ActivateObject` returns 0x8000FFFF on RTX 5090 Blackwell for H.264 / HEVC / AV1 MFTs regardless of adapter binding. Cascade routes around it (H.264+HEVC land on alternative MFTs; AV1 has no alternative and is filtered from advertised caps by the probe-at-startup check). Worth re-testing on newer drivers / `CODECAPI_AVEncAdapterLUID` experiments.
- [MEDIUM] [2026-04-22] Browser viewer: Chrome's `<video>` enforces a ~80 ms jitter-buffer floor regardless of `jitterBufferTarget=0` / `playoutDelayHint=0`. Partial workaround shipped (opt-in WebCodecs canvas render path, Chrome-only) — flip on by default once field hours accumulate.

## Security Baseline

- JWT expiry: access=604800s (7 days), refresh=2592000s (30 days) (configurable via ROOMLER__JWT__*).
- Rate limiting: tower_governor 60 req/min per IP (2026-03-21).
- CORS: configured via `cors_origins` (2026-03-21); see Known Issues for the `Any`-origin fallback when `cors_origins` is unset.
- nginx security headers: X-Frame-Options, X-Content-Type-Options, Referrer-Policy, Permissions-Policy (2026-03-21). HSTS + CSP still missing — Known Issues.
- TURN `static-auth-secret` rotated out of the repo on 2026-05-23 — the committed `turnserver.conf` carries a `CHANGE-ME` placeholder; the live value lives in the operator's `ROOMLER__TURN__SHARED_SECRET` env.
- `Content-Disposition` filenames sanitized + RFC 5987 encoded on the file-download route (2026-05-23).
- Last CVE scan: not yet run.
