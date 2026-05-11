# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**Roomler AI** is a real-time collaboration platform with chat, video conferencing, file sharing, room management, and a TeamViewer-style remote desktop subsystem. Stack: Rust (Axum) + MongoDB + Vue 3/Vuetify 3 + Pinia + Mediasoup (WebRTC SFU) + webrtc-rs (P2P remote-control). The remote-control subsystem ships as a separate native agent binary (`roomler-agent`) that runs on controlled hosts — see `docs/remote-control.md` and `HANDOVER2.md`.

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

The agent picks an encoder at startup via a three-way preference: **CLI `--encoder` > env `ROOMLER_AGENT_ENCODER` > `encoder_preference` in the agent config TOML > `Auto` default**. Values: `auto` | `hardware` (aliases: `hw`, `mf`) | `software` (aliases: `sw`, `openh264`).

- `Auto` (default): on Windows with `mf-encoder` feature, `MF H.264 (probe-and-rollback cascade) → openh264 → Noop`. Everywhere else, `openh264 → Noop`. Capture downscales 1440p/4K with a 2× box filter before encode.
- `Hardware` (Windows only, requires `--features mf-encoder` / `full-hw`): MF H.264 → openh264 → Noop. Same cascade as Auto, just ignores the `ROOMLER_AGENT_HW_AUTO=0` escape hatch.
- `Software`: openh264 → Noop. Forces the SW path even on Windows with `mf-encoder` compiled in — useful as a quick comparison escape hatch.

**Escape hatch**: `ROOMLER_AGENT_HW_AUTO=0` (or `false` / `no` / `off`) reverts Auto to openh264-first on Windows without a rebuild. Intended for diagnosing regressions in the field; no effect on `Hardware` or `Software` preferences.

The MF cascade (landed in 0.1.26) walks DXGI adapters × enumerated H.264 MFTs, applies `MF_TRANSFORM_ASYNC_UNLOCK` unconditionally (the MS SW MFT silently delegates to async HW on systems with installed drivers), tolerates `SET_D3D_MANAGER` returning `E_NOTIMPL` (treats the candidate as a sync CPU MFT), and runs a 480×270 NV12 probe frame per candidate. Async-only MFTs that ignore the unlock (Intel QSV) route to `MfInitError::AsyncRequired` and will be picked up by the async pipeline (Phase 3 commit 1A.2) once it lands. The final fallback inside the cascade is still the default-adapter SW MFT, so any working `CLSID_MSH264EncoderMFT` produces output.

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
- Docker: `docker-compose.yml` runs MongoDB 7 (auth: roomler/R00m1eR_5uper5ecretPa55word), Redis 7, MinIO, coturn
- Default DB URL: `mongodb://localhost:27019` (tests use no auth)

## Deployment

- **Production URL**: `https://roomler.ai/` — the live deployment. Use this as the `--server` argument when enrolling agents and as the origin the browser controller loads.
- **Docker**: Multi-stage build (rust:1.88-bookworm -> oven/bun:1 -> debian:trixie-slim + nginx)
- **Deploy repo**: `/home/gjovanov/roomler-ai-deploy/` on mars. Kustomize manifests live under `k8s/base/` + `k8s/overlays/prod/`. Ansible playbooks retained for host-level tasks only (HAProxy, WireGuard, iptables).
- **GitOps**: ArgoCD at [argocd.roomler.ai](https://argocd.roomler.ai) reconciles the `roomler-ai` Application from `github.com/gjovanov/roomler-ai-deploy @ master` path `k8s/overlays/prod`. Sync policy is **Automated + selfHeal + prune** with a GitHub webhook on the deploy repo: `git push` to master rolls out within ~5 s. 60 s polling fallback via `argocd-cm.timeout.reconciliation: 60s`. The 8 Application CRDs (bauleiter / lgr / oxmux / purestat / regal / roomler-ai / roomler-old / tickytack) are gitops-managed at `github.com/gjovanov/argocd-apps`; an `argocd-apps` parent app-of-apps watches that repo and reconciles `apps/*.yaml`. Verify the live targetRevision with `argocd app get roomler-ai --grpc-web | grep -E "Target|Sync Status"`.
- **Image registry**: `registry.roomler.ai` (self-hosted Docker Registry v2 on mars, basic auth, cert auto-renewed via acme.sh). Pull secret `regcred` lives in the `roomler-ai` namespace.
- **K8s cluster**: 3 control-plane + 3 worker nodes (Ubuntu 22.04, containerd 1.7.29, v1.31.14). Three zones via `topology.kubernetes.io/zone`: `mars`, `zeus`, `jupiter` (one master + one worker VM per bare-metal host).
- **Tier policy** (added 2026-05-01): cluster nodes are labelled `tier=high-performance` (zeus + jupiter workers) and `tier=utility` (mars worker). roomler-ai schedules on `tier=high-performance` only — never on mars worker. Enforced via a Kustomize patch in `roomler-ai-deploy/k8s/overlays/prod/kustomization.yaml` (commit `dab3cfa`) that adds a required `nodeAffinity` to every Deployment + StatefulSet. Hostname pin in `base/` (`kubernetes.io/hostname: k8s-worker-3`) is intentionally retained — the StatefulSet PVCs use node-local storage, so the data lives on jupiter; the tier requirement is an *additional* constraint, both must match. **mars worker hosts**: monitoring (kube-prometheus), `registry.roomler.ai`, image builds (direct on the host), `bauleiter`, `regal`. **High-perf hosts (zeus + jupiter)**: roomler (old), roomler-ai, oxmux, clawui (when migrated to K8s), lgr, purestat, tickytack.
- **Pod placement**: roomler-ai's pods run on `k8s-worker-3` (10.10.30.11, jupiter). Namespace `roomler-ai`, deployment `roomler2` (note: name is `roomler2` not `roomler-ai`), Recreate strategy, hostNetwork, `imagePullPolicy: IfNotPresent`.
- **Health probes**: startup/readiness/liveness all on `/health` (port 80 via nginx -> :3000 backend)
- **nginx**: Pod-internal reverse proxy (`files/nginx-pod.conf`) — SPA fallback + API proxy + WS proxy
- **Agent binary**: built separately (`cargo build -p roomler-agent --release --features full`) and distributed to controlled hosts via GitHub Releases (MSI / .pkg / .deb auto-built by `.github/workflows/release-agent.yml` on `agent-v*` tag push). Not part of the API Docker image.

### K8s deploy pipeline (ArgoCD GitOps)

Mars builds the image, pushes to `registry.roomler.ai/roomler-ai:<tag>`, bumps the tag in the gitops repo, and ArgoCD reconciles the Deployment.

```bash
ssh mars
cd /home/gjovanov/roomler-ai && git pull
docker build -t registry.roomler.ai/roomler-ai:build-$$ .           # ~5–15 min (cache warm)
TAG="v$(date +%Y%m%d)-$(docker images -q registry.roomler.ai/roomler-ai:build-$$ | head -c 12)"
docker tag registry.roomler.ai/roomler-ai:build-$$ registry.roomler.ai/roomler-ai:$TAG
docker tag registry.roomler.ai/roomler-ai:build-$$ registry.roomler.ai/roomler-ai:latest
docker push registry.roomler.ai/roomler-ai:$TAG
docker push registry.roomler.ai/roomler-ai:latest

cd /home/gjovanov/roomler-ai-deploy
git checkout master && git pull
sed -i "s|newTag:.*|newTag: $TAG|" k8s/overlays/prod/kustomization.yaml
git commit -am "chore(k8s): bump roomler-ai to $TAG"
git push

argocd app sync roomler-ai --grpc-web     # or Sync via argocd.roomler.ai UI
curl -sI https://roomler.ai/health        # HTTP/2 200
```

Registry retention: `/home/gjovanov/.local/bin/registry-retention.sh 1` (weekly cron at Sun 04:00) keeps at most 2 tags per repo (latest + most-recent-versioned) and GC's the registry storage.

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

## Remote Control Subsystem

TeamViewer-style remote desktop. One native agent per controlled host, Roomler API as signalling-only relay, browser as controller. All media + input flows over direct WebRTC P2P (TURN-relayed if needed) — the server never sees raw pixels or keystrokes.

**Design + architecture**: `docs/remote-control.md` (16 sections covering goals, topology, protocol, data model, security, latency budget).

**Resumption note after a session break**: `HANDOVER16.md` is the most recent — captures the rc.19 agent-side completion (P0-P3 + P6, 5 commits on master, NOT YET TAGGED) plus the queued browser work (P4-P5 + P7 + smoke + tag). Read this first when picking up rc.19. `HANDOVER15.md` covers the rc.16 → rc.18 cycle (file-DC v2 follow-ons, operator-consent, mobile keyboard, perMachine UAC, cross-flavour cleanup, Tauri tray). `HANDOVER2.md` → `HANDOVER14.md` trace the historical arc. **For the current resilience-cycle state (0.1.50 → 0.1.54), read `docs/remote-control.md` §19 — the agent now self-heals from crashes, verifies update integrity, auto-rolls-back on broken releases, and registers its Scheduled Task at MSI install time without operator intervention.**

**Wire protocol**: `rc:*` JSON messages over the existing `/ws` endpoint. `ClientMsg` / `ServerMsg` in `crates/remote_control/src/signaling.rs`. ObjectIds are raw hex strings (locked by tests); `Permissions` serialises as pipe-separated names (bitflags 2.x convention, also locked).

**WebSocket role multiplexing**: `/ws?token=<jwt>&role=agent` uses the agent JWT audience; no `role` param (or `role=user`) uses the existing user flow. Same WS endpoint, same handshake, different claim validator.

**Status at 0.3.0-rc.18** (UAC self-update + cross-flavour MSI cleanup + config migration + Ctrl+C auto-mirror + viewer focus-stealing fix + Tauri tray companion, 2026-05-11):
- **perMachine `self-update` surfaces UAC** (Feature 1). New `msiexec_argv(flavour)` pure helper dispatches argv per flavour: perUser keeps `/qn` (silent, no UAC); perMachine uses `/qb!` (basic UI, allows UAC). The Windows branch of `spawn_installer_inner` now routes perMachine through `ShellExecuteExW` + `verb="runas"` so a non-elevated caller (Scheduled Task / interactive shell) prompts for consent before msiexec gets the admin token. Bails with a clear "UAC consent declined" message when the user clicks No. Manual `self-update` CLI now uses `spawn_installer_with_watch(..., Some(latest))` so every install attempt produces a `last-install.json` trail (closes the diagnostic gap that hid the rc.17 silent failure on PC50045). 4 new `msiexec_argv` unit tests.
- **Cross-flavour MSI cleanup** (Feature 2). New hidden CLI subcommand `roomler-agent cleanup-legacy-install --target-flavour {perUser|perMachine} [--dry-run]`. Removes the OPPOSITE flavour's leftovers — Scheduled Task `RoomlerAgent`, SCM service `RoomlerAgentService`, data dirs under `%LOCALAPPDATA%` / `%APPDATA%` / `%PROGRAMDATA%`. The perMachine→perUser direction reaches into the active-session user's profile via the existing `system_context::user_profile::active_user_profile_root()` helper (no new SYSTEM-context plumbing). Same-flavour invocations fast-path-exit-0. Both WiX files dropped the cross-flavour LaunchCondition refusal — operators can now switch flavours freely; the new deferred custom action (`CleanupLegacyPerMachine` / `CleanupLegacyPerUser`) scrubs first, then InstallFiles proceeds. `Return="ignore"` so a cleanup glitch doesn't sink the install.
- **Explicit config migration** (Feature 3). New `config::migrate(cfg) -> bool` and `config_schema_version` field. Called from `run_cmd` immediately after `config::load`. On a pre-rc.18 config, trims trailing slashes from `server_url`, resets `crash_count` if `last_known_good_version` is from a pre-rc.18 branch (so crashes-on-0.2.x don't trip rc.18 rollback), and stamps `config_schema_version = Some("0.3.0-rc.18")` so subsequent launches no-op. 6 new migration unit tests. Forward-compat: future rc.19+ migrations key off the stamped version.
- **Auto Ctrl+C over canvas** (Feature 4). When pointer is over the viewer + operator hits Ctrl+C, the HID keystroke is forwarded as before AND — after a 25 ms delay — the browser auto-reads the host's clipboard (via the existing `getAgentClipboard` clipboard-DC round-trip) and writes the result to `navigator.clipboard.writeText`. Fallback path surfaces a snackbar with the text + a manual Copy button when the browser refuses writeText (no user-gesture chain). Ctrl+V's existing deferred-keystroke design was already correct for the reverse direction. Toolbar buttons "Send my clipboard - remote" / "Get remote clipboard - me" remain but become redundant for keyboard users.
- **Viewer focus-stealing fix** (Feature 5). Field bug: clicking a left-panel nav item (Dashboard / Rooms / Files / etc.) then connecting to the remote viewer left that `<v-list-item>` focused; the next Enter / Space pressed over the viewer fired Vuetify's keyboard-activation `@click` and navigated away. Fix: `attachInput` accepts a `focusAnchor: HTMLElement` option (the viewer canvas wrapper, already has `tabindex="0"`). On `pointerenter`, the composable blurs `document.activeElement` then focuses the anchor. Window keydown listeners switched to **capture phase** with `stopPropagation` when pointer is inside the viewer — focused descendants don't see the key. Outside the viewer, normal browser shortcuts (Tab, Esc, Ctrl+T) still work because stopPropagation is gated on `pointerInside`.
- **Tauri tray companion** (Feature 6 — new `agents/roomler-agent-tray/` workspace member). Small Tauri 2.x app providing onboarding GUI (paste enrollment token + device name → calls `roomler_agent::enrollment::enroll` as a direct lib call), status view (service state, agent version, attention sentinel), and a system-tray icon with a right-click menu (Open Status / Onboarding / Check for Updates / Open Logs Folder / Quit). Static HTML/CSS/JS front-end (no bundler). Service control, self-update trigger, and consent decisions shell out to the existing `roomler-agent` CLI subcommands — no socket / HTTP IPC, just file-sentinel pattern (consent dir + needs-attention.txt) the agent already uses. `release-agent.yml` builds a tray EXE artifact alongside the agent MSIs on Windows for rc.18; tray MSI bundling deferred to rc.19.
- **Open**: tray Linux build (Windows + macOS only in rc.18); tray MSI / .pkg bundling via `cargo tauri build` (today's artifact is plain EXE; operators copy alongside the agent install dir); SCM-service-driven BG auto-update for perMachine (deferred to rc.19 once we have telemetry on fleets where UAC consent in-the-loop is unworkable).
- **Field bug 2026-05-11** (motivates rc.19): 35 MB .xlsx upload from the browser → PC50045 failed at 4 % (1,376,256 / 36,268,790 bytes) with the existing "files channel closed mid-upload … remote agent restarted" error. Most likely cause is the agent's auto-update timer firing mid-transfer (perMachine self-update spawns msiexec + exits → WebRTC peer + file-DC die). User chose **resumable transfers** for rc.19 (over retry-from-byte-0) since multi-MB uploads make re-uploading from 0 unacceptable. See `HANDOVER16.md` for current state.
- **Tests**: 252 agent lib tests green at end of rc.18 (+10 new — 4 msiexec_argv + 2 install_cleanup + 6 config migration); 130 frontend Vitest cases (unchanged — no new front-end coverage; closures inside `attachInput` not unit-isolated).

**Status at rc.19 — agent side shipped on master, browser pending, NOT YET TAGGED** (5 commits between `0f109b1` and `b23e9ea`, 2026-05-11):
- **Wire format** (P0): new `FilesIncoming::Resume { id, offset, sha256_prefix }` + `FilesOutgoing::Resumed { id, accepted_offset }` variants in `agents/roomler-agent/src/files.rs`. Caps add `"resume"` to `caps.files` so browsers opt in only when supported. `PARTIAL_REGISTRY: LazyLock<Mutex<HashMap<id, meta_path>>>` and `ACTIVE_TRANSFERS: AtomicUsize` globals.
- **Agent staging** (P1): every upload (when agent has resume cap, which is always in rc.19) writes to `<dest_dir>/.roomler-partial/<id>/data` with a sibling `meta.json` describing the transfer. `chunk()` calls `sync_data()` per 1 MiB (B2 fix tuned for Windows Defender — 35 syscalls per 35 MB upload, ~35-1050 ms overhead). `begin()` rejects duplicate ids on disk (B1 fix — prevents `File::create` truncating an existing partial). `sweep_orphans()` runs synchronously at agent startup BEFORE the WS signaling task spawns; walks `Downloads/.roomler-partial/`, deletes dirs older than 24h, registers survivors. `end()` re-runs `unique_path()` at rename time so operator file-ops mid-upload don't clobber the final (M4 fix). Same-volume rename → O(1).
- **Resume handler** (P2): `FilesHandler::resume_incoming(id, offset)` looks up the partial via `PARTIAL_REGISTRY` (on-demand stat at canonical Downloads-rooted path as fallback), validates `meta.json`, truncates the staging `data` file to `min(offset, disk_size) & !(256 KiB - 1)` (B3 fix — alignment matches `files:progress` cadence), reopens for append, reinstalls state in the calling DC's `incoming` Mutex. Cancel arm extended to upload-side — removes the per-id staging dir + registry entry so browser-driven terminal failures don't leak partials until 24h sweep.
- **Auto-update gate** (P3): RAII `ActiveTransferGuard` field on `IncomingTransfer`/`OutgoingTransfer` (last field — drops AFTER the file handle so kernel-flush precedes counter decrement). `updater::run_periodic` calls pure `decide_defer(active, consecutive_defers)`: defers 1h-cycle when `active > 0`, forces update after 7 consecutive defers (M3 fix — bounded ~7h delay for chronically-busy hosts).
- **Tests**: 272 agent lib tests green (+15 new — 4 serde locks + 5 P1 staging + 5 P2 resume + 4 P3 defer + 1 caps). 13 file_dc integration tests (+1 — `files:resume unknown id → files:error`). Clippy + fmt clean. The DC-recreate full-resume integration test was prototyped but deferred: webrtc-rs loopback SCTP teardown races the second DC's chunk loop on Windows. Lib tests cover the mechanics by driving `FilesHandler::resume_incoming` directly.
- **Browser side pending** (P4-P5, P7): `useRemoteControl.ts` does NOT import `useAgentsStore` today — composable gains an `agent` arg from `RemoteControl.vue`. `uploadOne` refactors into `innerPump` (re-reads live `channels.files` per attempt — existing capture of the dead channel is P0-3 bug) + outer `uploadOneResumable` with 6-attempt budget. `pendingResumePromises` Map mirrors `pendingDirRequests` shape so `files:resumed` replies route correctly. `RemoteControl.vue:332` v-if extends to `phase === 'reconnecting'` so the badge isn't invisible.
- **Open**: P4 + P5 (browser auto-resume), P7 (UI polish), P8 (manual smoke on PC50045 — kill at 4/25/50/90%, auto-update mid-flight, sync_data perf re-measure), P9 (tag `agent-v0.3.0-rc.19`). Plan at `~/.claude/plans/floating-splashing-nebula.md`.

**Status at 0.2.6** (auto-update asset-picker fix shipped 2026-05-02 on top of 0.2.5):
- **Asset-picker now flavour-aware**. Pre-0.2.6 `pick_asset_for_platform` returned the first `.msi` in the GitHub release asset listing — alphabetical order which puts `…-perMachine-…msi` ahead of the plain `…-x86_64-…msi`. A perUser agent on 0.2.0 polling the 0.2.5 release downloaded the perMachine MSI, msiexec ran it `/qn`, the cross-flavour launch condition silently rejected the install, the parent agent exited expecting the installer to win, the Scheduled Task respawned the same 0.2.0 binary — auto-update made zero forward progress for any agent in the field. Field repro: PC50045 (e069019l) 2026-05-02. **Fix**: new `WindowsInstallFlavour` enum + `current_install_flavour()` reads `std::env::current_exe()` and classifies as `PerMachine` if under `\Program Files (x86)?\…`, else `PerUser`. New `pick_asset_for_windows(assets, flavour)` filters MSIs by `-perMachine-` infix, falls back to "any MSI" only if the matching flavour is missing. 7 new tests pin the contract (perUser skips perMachine even when alphabetically first; flavour-discovery from typical Windows install paths; case-insensitive; both fallback directions).
- Manual upgrade workaround for any agent still on 0.2.0–0.2.5: download the **perUser** MSI (no `-perMachine-` infix) directly from the GitHub Releases page and run it. From 0.2.6 forward auto-update self-heals.

**Status at 0.2.5** (M3 cycle locked at Z-path + WGC NO-GO confirmed 2026-05-02):
- **0.2.5 ships everything** that was substantively in 0.2.1 / 0.2.2 / 0.2.3 / 0.2.4 (whose CI all failed): perMachine MSI with auto-SCM-service registration + mutually-exclusive launch conditions, input drop-on-lock, `rc:host_locked` control-DC signal + viewer toolbar badge, plus the WiX fixes that finally let it build (drop ApplicationShortcut from perMachine MSI to satisfy ICE38/43/57; relocate perMachine WXS to `agents/roomler-agent/wix-perMachine/main.wxs` so cargo-wix's per-User scan doesn't compile it).
- **A1 (real Winlogon SYSTEM-context capture+input) NO-GO via WGC** — empirically confirmed 2026-05-02 on PC50045 via `psexec -s -i 1 ...\roomler-agent.exe system-capture-smoke --desktop winlogon`. The smoke binary cleanly proved every upstream piece works under SYSTEM (`OpenDesktop("Winlogon")` + `SetThreadDesktop` + D3D11 + IDirect3DDevice + monitor enum), then `IGraphicsCaptureItemInterop::CreateForMonitor` died with HRESULT `0x80070424` = `HRESULT_FROM_WIN32(ERROR_SERVICE_DOES_NOT_EXIST)` = WGC's WinRT activation chain can't reach a service from session 0. Consistent with RustDesk and other remote tools — Microsoft has never officially supported WGC from SYSTEM. M3 cycle is locked at the Z-path overlay; remote-unlock is a future-investigation item.
- **A1 follow-up investigations queued for next session** (see `HANDOVER11.md` and the `project_m3_a1_investigation.md` memory): (a) reproduce RustDesk's approach to lock-screen capture+control to learn which APIs / services / context they use; (b) probe the missing-service path — try starting `AppXSvc` / `tabletinputservice` / others; (c) DXGI Desktop Duplication fallback as the third option if (a) and (b) yield no path forward.

**Status at 0.2.4** (CI-fix shipped 2026-05-02 — 0.2.1 / 0.2.2 / 0.2.3 release builds all failed):
- **CI fix only** — no behaviour change. The per-Machine MSI work in 0.2.1 placed `main-perMachine.wxs` next to `main.wxs` inside `agents/roomler-agent/wix/`, but cargo-wix auto-discovers every `*.wxs` in that dir so the per-User MSI build also tried to compile the perMachine source. That tripped a separate WiX 1.0 rule (`--` not allowed inside comments) on `--include` and `--as-service` text in the perMachine file's prose, which had been visible since the v3 git mv. **Fix**: relocate the perMachine source to a sibling `agents/roomler-agent/wix-perMachine/main.wxs` directory (outside cargo-wix's scan path), update the release-agent.yml swap step to copy from the new location, paraphrase any `--` runs that survived in comments. 0.2.0 release pipeline was the last green build before 0.2.4.

**Status at 0.2.3** (rc:host_locked control-DC signal + viewer badge shipped 2026-05-02 on top of 0.2.2):
- **`rc:host_locked` control-DC message**: agent emits a JSON envelope `{"t":"rc:host_locked","locked":true|false}` over the existing `control` data channel on every lock-state transition (and once at session start to seed the initial state). The browser's `useRemoteControl.ts` exposes a `hostLocked` ref that the viewer renders as a yellow `mdi-lock` chip in the toolbar. Supplements the in-stream padlock overlay frame for operators whose video element is scrolled out of view or who are taking screenshots for support. Backward-compatible: older agents never emit the message; the badge stays hidden and the experience falls back to overlay-only.

**Status at 0.2.2** (input suppression on lock shipped 2026-05-02, polish on top of 0.2.1):
- **Input drop-on-lock**: `attach_input_handler` now consumes the `lock_state` watch receiver and drops InputMsg dispatches early when `LockState::Locked`. Previously the events were JSON-parsed and forwarded to the OS injector, where SendInput silently routed them to `winsta0\Default` while the input desktop was on `winsta0\Winlogon` — appeared to work at the WS layer but achieved nothing on the host. Now the early drop keeps the audit trail honest and avoids polluting `enigo` internal state. Logs every 60th suppressed event at debug level so the field gets a steady "yes, suppression is working" signal without flooding.

**Status at 0.2.1** (per-Machine MSI flavour shipped 2026-05-02, on top of 0.2.0):
- **Per-Machine MSI** (M3 phase 4): a second WiX source `wix/main-perMachine.wxs` builds a parallel MSI with `InstallScope='perMachine'`. Files land under `%ProgramFiles%\roomler-agent`; install runs elevated (UAC fires once); a deferred + non-impersonated `RegisterService` custom action auto-runs `roomler-agent service install --as-service` so the SCM service is registered in one msiexec invocation. Symmetric `UnregisterService` on uninstall. The release-agent.yml workflow now builds both MSIs from one tag (file-swap of `wix/main.wxs` between the two `cargo wix` runs); both upload to the GitHub Release with distinct filenames (`-perMachine-` infix).
- **Mutually-exclusive launch conditions**: each MSI carries an `<Upgrade OnlyDetect='yes'>` element targeting the OTHER flavour's UpgradeCode, plus a `<Condition>` blocking install if the other was found. Closes the SHOWSTOPPER concern from the M3 plan critique: two binaries / two updaters / two autostart hooks on one host is silent corruption on update; the conditions force operators to pick one flavour.
- **packaging/windows/README.txt** updated with the two-MSI choice (Option A: perMachine for fleet, Option B: perUser + manual elevation).
- Default behaviour for everyone not opting in is unchanged: the per-User MSI still ships, still installs without UAC, still registers the Scheduled Task auto-start. The new perMachine MSI is purely additive.

**Status at 0.2.0** (M3 Z-path lock-screen overlay + browser auto-reconnect + mobile-friendly viewer shipped 2026-05-02, minor version cut to mark the M3 milestone):
- **Z-path lock-screen overlay** (M3 phase 3): user-context worker now polls `OpenInputDesktop` every 500 ms via `lock_state.rs`. ACCESS_DENIED (or any non-"Default" desktop name) flips a `tokio::sync::watch::Sender<LockState>` to `Locked`. The capture/encode pumps in `peer.rs` (both legacy track + VP9-444 DC) substitute the real-but-stale capture with a synthesised "Host is locked" overlay frame at the same dimensions — a centred yellow padlock badge on dark grey, painted from rectangles (no font crate, ~5 KB code, ~10 KB H.264 keyframe). Both transitions force a keyframe so the browser decoder snaps the new frame in immediately. Closes the field bug confirmed on PC50045 + e069019l 2026-05-02 ("cannot enter username and password in win logon screen via the remotely controlled screen") with the simplest dignified pause: operator sees a clear "agent paused, host is locked" visual instead of frozen black, and the WebRTC peer stays connected throughout. **Z-path explicitly does NOT support remote unlock** — that's the A1 follow-up which depends on the Winlogon WGC capture spike (still pending; see `roomler-agent system-capture-smoke --desktop winlogon` via psexec).
- **Browser auto-reconnect ladder** (M3 phase 2): `useRemoteControl.ts` schedules a reconnect on peer `connectionState === 'failed'` instead of failing terminally. Backoff 250ms → 500ms → 1s → 2s → 4s → 8s, capped at 6 attempts (~16 s worst case). First three steps tuned for desktop-transition recovery (sub-second), last three for real network drops. Only `'failed'` triggers retry; `'disconnected'` is transient ICE checking. Successful `'connected'` resets the counter. Operator-initiated `disconnect()` and terminal `failWith()` both cancel the timer. Toolbar shows "Reconnecting (N/6)…" in a warning chip while retrying.
- **`SpawnDecision::SystemContextCapture` variant** (M3 phase 1): supervisor's `decide_spawn` now takes a third `keep_stream_alive` argument distinguishing "tear down worker fully" from "swap to SYSTEM-context capture+input thread". Today the supervisor always passes `false` (M2-equivalent behaviour); the variant exists so M3 phase 4 can wire the SCM-side capture path without another API churn.
- **Mobile-friendly viewer toolbar**: the four advanced selects (Quality / Scale / Resolution / Codec, ~630 px combined) hide on `<md` and surface in a `<v-bottom-sheet>` via a new `mdi-tune-variant` icon button. Agent title truncates; OS+version subtitle hides on phone. Connect/Disconnect always reachable. Closes the field complaint that the viewer was unusable on a phone.
- **Test surface**: 177 agent-lib tests (was 162 in 0.1.61) — 6 lock_state, 8 lock_overlay, 1 new SystemContextCapture variant test. 363 frontend tests — 7 new for the reconnect ladder shape + `nextReconnectDelayMs` bounds.
- **Open**: A1-path (real Winlogon SYSTEM-context capture+input via WGC + named-pipe IPC) gated on the Winlogon WGC spike result; per-machine MSI flavour with launch-condition; user-bridge IPC for clipboard during SYSTEM-mode. All deferred to a 0.2.x cycle.

**Status at 0.1.63** (cooldown log visibility fix shipped 2026-05-02, on top of 0.1.62):
- **Tiny visibility patch** to `updater::run_periodic`: the "suppressed by recent-install cooldown" log line is now emitted BEFORE the 24h interval sleep, not after. The 0.1.62 ordering meant the cooldown silently did its job but the announcement only made it to disk on the next wake-up — verifying the fix by `Select-String "suppressed by recent-install"` failed within the 5-min window even though the storm had been prevented. Field repro: e069019l 2026-05-02 (worker spawned at 16:21:11Z, killed at 16:23:34Z, supervisor respawned without "agent starting" entry between 16:21:11Z and 16:24:13Z — 3-min gap = cooldown working — but no "suppressed" line existed because we hadn't slept past the 24h mark yet). Fix: log on the *decision*, not after the sleep.

**Status at 0.1.62** (auto-update install-storm prevention shipped 2026-05-02 on top of 0.1.61):
- **Install-storm fix** (0.1.62): the `updater::run_periodic` loop now reads a `<log_dir>/update-attempt` marker file on its first iteration; if the marker's mtime is < 5 min old (`STARTUP_UPDATE_COOLDOWN`), the at-startup check is suppressed and the loop falls through to the periodic interval. The marker is written by `spawn_installer_with_watch` *before* the installer process is launched. Field repro that drove this fix: e069019l 2026-05-02. After a fresh boot of an 0.1.60 host with 0.1.61 published, each SCM-supervised worker spawn (~1.5 s lifetime) detected the same pending update, fired another MSI installer, exited cleanly with code=0, supervisor respawned, repeat — install-storm of ~25 worker spawns + 25 concurrent MSIs in 100 s. The 0.1.61 supervisor's `code=0 → no backoff` patch made the cycle tighter (1.5 s vs 3.5 s under 0.1.60). With the cooldown, a fresh worker spawned by the supervisor during an in-flight install now skips the at-startup check, runs normally, and waits 24 h before re-checking.
- **5 new updater tests** lock the cooldown contract: marker missing → false; marker fresh → true; cooldown=0 → false; marker old → false; `STARTUP_UPDATE_COOLDOWN == 300 s` constant pinned. 162 agent-lib tests green.

**Status at 0.1.61** (M5 verification + clean-exit fixes shipped 2026-05-02, on top of 0.1.58 below):
- **M5 verification** (0.1.61): SCM service mode driven through a real install-and-cycle on PC50045 via the new `agents/roomler-agent/scripts/m5-verify-win11.ps1` (Status / Install / Restart / Rollback / Logs / SystemLogs / Smoke actions). Confirms the M2 architecture works as designed: SCM service host + user-context worker spawned via `WTSQueryUserToken` + `CreateProcessAsUserW`, with the post-restart respawn cycle reproducible end-to-end. **M3 gap empirically confirmed**: 4 SessionChange events from Win+L lock+unlock arrive at the supervisor but `WTSGetActiveConsoleSessionId` stays = 1 across lock, so `decide_spawn` returns `KeepCurrent` and the worker never follows focus into `winsta0\Winlogon`. Locked the M5 baseline before touching M3.
- **Clean-exit-not-a-crash fix** (0.1.61): both the user-mode crash counter and the SCM supervisor's `consecutive_failures` ladder were treating intentional clean exits (auto-update self-shutdown, instance-lock-race after SCM restart) as failures. M5 caught three concrete cases: (a) user-mode false-positive `crash_count++` after every auto-update restart (would trip the rollback threshold after 3 rapid updates); (b) SCM supervisor flagging the auto-updater's worker exit as `consecutive_failures=1, backoff_secs=2`; (c) SCM supervisor's own restart racing the previous worker's instance-mutex release (145 ms between Stop+Start, lock not yet released, new worker exits cleanly with code=0, supervisor counts it as a crash). One root cause, one fix: new pure `decide_exit_reaction(code, counter)` returns `Respawn` for code=0 and `Backoff(d)` for non-zero; main.rs marks runs graceful when `sig_task` exits via the internal `shutdown_tx` signal; supervisor's three terminate paths now `wait_for_exit(1500ms)` so the OS reaps the process before the next spawn. Locked by 3 new unit tests; 152 agent-lib tests green.
- **Open** (still): M3 (pre-logon SYSTEM-context capture — A1 architecture chosen, WGC session-0 spike pending), per-machine MSI flavour with launch-condition blocking dual-install. Both ride the upcoming 0.1.62 + 0.2.0 releases.

**Status at 0.1.58** (Phase 7 + 8 + Effort 2 M1+M2 cycle shipped 2026-04-30, on top of the 0.1.54 baseline below):
- **Field 405 fix** (0.1.55): `enrollment::normalize_server_url` upgrades operator-supplied `http://` → `https://` upfront so the cluster's HTTP→HTTPS 301 doesn't downgrade POST→GET → 405 Method Not Allowed. Stored config keeps https/wss for the long-lived signaling connection. Bonus security win: enrollment tokens never leave the wire in cleartext.
- **Phase 7 heartbeat telemetry** (0.1.56): agents emit `ClientMsg::AgentHeartbeat { rss_mb, cpu_pct, active_sessions }` every 30 s on the existing `/ws`; backend WS handler calls `agents.touch_heartbeat(agent_id)` to refresh `last_seen_at`. Closes the "agent shows online forever after silent disconnect" gap. v1 sends rss=0 / cpu=0.0 (defer sysinfo dep); active_sessions is the live peer-map size. Backend deployed via mars rebuild → ArgoCD GitOps in the same cycle.
- **Phase 8 pre-flight checks** (0.1.57): new `preflight` module runs DNS + TCP + clock-skew probes in parallel right after config load. Each finding is a `warn!` with an actionable `hint=` field; non-blocking, ~5 s total wall, 5 s per probe. Catches the most common deployment blunders (firewall, captive portal, NTP drift) before the WS reconnect ladder masks them.
- **Effort 2 M1+M2** (0.1.58): optional opt-in Windows Service deployment mode. `service install --as-service` registers `RoomlerAgentService` with the SCM (LocalSystem, AutoStart) via the new `windows-service`-backed `win_service` module. Service supervises a per-session worker via `WTSQueryUserToken` + `CreateProcessAsUserW` (raw FFI through `windows-sys = "0.59"`); SCM SessionChange notifications swap workers on logon/logoff. Worker crash → respawn under 2 s → 60 s exponential backoff (parity with Scheduled Task `RestartOnFailure`). Default behaviour (Scheduled Task) unchanged for everyone not opting in.
- **M4 deferred-by-design**: the agent MSI is `InstallScope='perUser'` so it runs without UAC; `CreateService` requires admin elevation. Auto-registering the SCM service from a perUser MSI is impossible. Operators install the MSI normally then run `service install --as-service` from elevated PowerShell. A future per-machine MSI flavour can revisit.
- **Open**: M3 (pre-logon SYSTEM-context capture — needs hands-on Win11 testing of WGC capture from session 0), M5 (verification on a real Win11 install), per-machine MSI flavour for the M4 fix-up.

**Status at 0.1.54** (resilience cycle 0.1.50 → 0.1.54 shipped 2026-04-29, preserved as baseline):
- **Failure-resilience P0** (0.1.50): persistent rolling logs + panic hook (`%LOCALAPPDATA%\roomler\roomler-agent\data\logs\`); Win Scheduled Task XML with `RestartOnFailure` (PT1M × 10) + `MultipleInstancesPolicy=IgnoreNew` + `StopIfGoingOnBatteries=false` + `DisallowStartIfOnBatteries=false` (parity with systemd `Restart=on-failure` / launchd `KeepAlive`); single-instance lock (Win named mutex `Local\` namespace per-session, Unix `flock`); internal liveness watchdog (signaling 90s threshold; encoder + capture pumps gated on session-active; 5s scan with 60s suspend tolerance; `std::thread` watchdog-of-watchdog catches a fully-deadlocked tokio runtime); token revocation grace (no more hard exit on 401; backoff 30s → 1h ladder; `needs-attention.txt` sentinel after 3 consecutive 401s; new `re-enroll --token <jwt>` CLI preserves machine_id + machine_name).
- **Update-path hardening** (0.1.51): configurable update interval via `update_check_interval_h` config field + `ROOMLER_AGENT_UPDATE_INTERVAL_H` env override; post-install watcher subprocess (hidden CLI `roomler-agent post-install-watch`) captures installer exit code + new-binary `--version` outcome to `<log_dir>/last-install.json`; AgentConfig crash-tracking fields (`last_known_good_version`, `crash_count`, `last_crash_unix`, `rollback_attempted`, `last_run_unhealthy` — all `#[serde(default)]` for back-compat); crash-loop detection raises operator-attention sentinel after 3 crashes within 10 min.
- **SHA256 + automatic rollback** (0.1.52): `verify_sha256` against GitHub's `digest` field (`"sha256:<hex>"` format added by Releases API in late 2024); proxy at `/api/agent/latest-release` forwards the field via `AgentReleaseAsset.digest`; mismatched downloads never touch disk (caught before write); `updater::pin_version(tag)` bypasses the proxy and fetches a specific release from GitHub directly; crash-loop detector now actually executes the rollback (downloads last-known-good installer, spawn_installer_with_watch, marks `rollback_attempted=true` to prevent infinite oscillation).
- **Schema 1.3+ regression fix** (0.1.53): removed `<DisallowStartOnRemoteAppSession>` and `<UseUnifiedSchedulingEngine>` from the `service install` XML — both are Schema 1.3+ elements that schtasks rejected in a Schema 1.2 document with `(39,7):DisallowStartOnRemoteAppSession: ERROR: The task XML contains an unexpected node`. Locked by a regression test.
- **MSI auto-registers Scheduled Task** (0.1.54): WiX `RegisterAutostart` custom action runs `service install` after `InstallFiles`; `UnregisterAutostart` runs `service uninstall` before `RemoveFiles`. perUser MSI runs in the user's token, so Impersonate=yes is the default — no UAC complications. `Return="ignore"` so an existing-task ACL conflict (the rare Win11 quirk) doesn't sink the install. Closes the gap that bit the field on 0.1.50–0.1.52: operators upgrading silently kept their pre-0.1.50 ONLOGON task with the bad battery defaults because the new XML shipped inside `service::install()` and never ran automatically.
- **CI hardening** (0.1.54 cycle): `continue-on-error: true` on the three `Cache cargo` steps in `.github/workflows/release-agent.yml`. The post-job tar/zstd cache write flaked on agent-v0.1.53 attempt 1 → marked the whole job as failure → `Publish GitHub Release` skipped despite every build step succeeding. Cache is an optimisation, not a correctness gate.
- **Open**: heartbeat telemetry to `agents.last_seen_at` (Phase 7), pre-flight checks (Phase 8), Windows Service deployment mode for fleet/unattended (Effort 2 with v1 pre-logon scope flipped on per 2026-04-29 directive).

**Status at 0.1.36** (preserved for context — superseded by the 0.1.54 status above):
- Server side: REST + WS signalling + Hub + DAOs + audit + TURN creds — complete, 10 integration tests green
- Agent binary: enrollment + signalling + real webrtc-rs peer + scrap capture + openh264 encoder + enigo input — **live-verified** on Win11 against the production deployment (2026-04-18)
- Browser viewer: RemoteControl.vue + useRemoteControl composable + AgentsSection admin UI — complete, letterbox-corrected coordinates, wallclock sample durations, idle-keepalive, PLI rate-limiting, **Scale modes** (Adaptive/Original/Custom), **Fullscreen** toggle, **Resolution override** (Original/Fit/Custom), codec-override dropdown, Ctrl+Alt+Del + clipboard + file-upload toolbar.
- Windows Media Foundation HW encoder (`--features mf-encoder` / `full-hw`): probe-and-rollback cascade complete (0.1.26). Adapter enumeration + per-MFT probe with blanket async-unlock and `SET_D3D_MANAGER E_NOTIMPL` tolerance. Auto prefers MF-HW on Windows. Async-only MFTs (Intel QSV) route to `AsyncRequired` for the upcoming async pipeline; today they fall through to the SW MFT final fallback cleanly.
- Codec negotiation (0.1.28+0.1.29+0.1.30): agent advertises H.264 + HEVC + AV1 caps via `AgentCaps.codecs` at `rc:agent.hello` time; browser advertises its decode caps via `ClientMsg::SessionRequest.browser_caps`; agent picks best intersection (priority: av1 > h265 > vp9 > h264 > vp8) and binds the matching MF encoder + `video/H264|H265|AV1` track + `set_codec_preferences` SDP pin. HEVC/AV1 failures are fail-closed (black video + WARN, not silent bitstream substitution). Caps probe-at-startup (0.1.30) filters codecs that enumerate-but-fail-to-activate (e.g. NVIDIA RTX 5090 Blackwell AV1 MFT).
- Data-channel handlers (0.1.32+0.1.33): clipboard round-trip (arboard, thread-pinned) and file-transfer (browser → host Downloads, 64 KiB chunks, `bufferedAmount` backpressure, filename sanitization, collision-safe rename) both complete. Cursor DC (0.1.31) streams real OS cursor shape+position at ~30 Hz; synthetic initials badge is the fallback for first-second-before-shape-cached. Hotkey interception (0.1.33): Ctrl/Cmd+A/C/V/X/Z/Y/F/S/P/R are locally `preventDefault`-ed while pointer is over the viewer; Ctrl+Alt+Del is a dedicated toolbar button (the OS reserves the real chord).
- Viewer-indicator overlay (0.1.33, Windows, `viewer-indicator` feature): topmost layered click-through window on the controlled host with a 6 px red border + "Being viewed by: …" caption; `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` keeps it out of the captured stream.
- RustDesk-parity Tier A (0.1.33): 60 fps + native resolution on the MF-HW path, bpp/s 0.10 → 0.15, MAX bitrate 15 → 25 Mbps, High cap 20 → 30 Mbps, browser `jitterBufferTarget=0 / playoutDelayHint=0 / contentHint='motion'`, `requestVideoFrameCallback` stall recovery + `play()` kicker, codec-override dropdown (persists per browser), Ctrl+Alt+Del toolbar button. Remaining plan: Tier B (WebCodecs canvas render path to bypass Chrome's ~80 ms jitter-buffer floor), Tier C (WebGPU / native Tauri companion app).
- Viewer scale + resolution controls (0.1.35): browser-side Scale modes (Adaptive / Original / Custom 5-1000%) + Fullscreen toggle + per-agent Remote-Resolution override (Original / Fit-to-local / Custom) with CPU box-filter downscale on the agent (`apply_target_resolution`) and `ResizeObserver`-driven auto-updates in Fit mode debounced 250 ms. `rc:resolution` control-DC message, no SDP renegotiation — the SPS/VPS carries the new size on the existing RTP track.
- Diagnostics (0.1.34): media-pump heartbeat reports `avg_capture_ms` / `avg_encode_ms` per ~30-frame window; WGC backend logs `wgc: capture cadence arrived=N drops=M drop_ratio_pct=P` every ~120 frames so the field can distinguish capture-starvation from encode-saturation without a profiler.
- WebCodecs render path (0.1.36, Tier B7): opt-in viewer toggle that routes the inbound video track through a Web Worker + `VideoDecoder` + `OffscreenCanvas` via `RTCRtpScriptTransform`, bypassing Chrome's built-in jitter buffer (~80 ms soft floor on `<video>`). `rc-webcodecs-worker.ts` owns the decode + paint; the main thread just swaps a `<canvas>` in for the `<video>` element when the user clicks the mdi-flash toolbar button. Chrome-only (falls back silently when `RTCRtpScriptTransform` or `VideoDecoder` are missing). Persisted per-browser; takes effect on next Connect. `shortCodecFromReceiver` reads the negotiated mime off `RTCRtpReceiver.getParameters()` so the decoder gets the right codec string without extra wire bytes.
- Agent lifecycle hooks (0.1.36): `roomler-agent service install|uninstall|status` registers a Scheduled Task on Windows (ONLOGON + LIMITED), a systemd user unit on Linux, or a LaunchAgent plist on macOS — idempotent, cross-platform, shells out to the OS tool. Background auto-updater polls `github.com/gjovanov/roomler-ai/releases/latest` every 6 h (+ at startup), downloads the platform installer (MSI / .deb / .pkg), spawns it detached, and exits so the installer can overwrite the binary; the service hook re-launches the new version on the next login. Disable via `ROOMLER_AGENT_AUTO_UPDATE=0`. Manual trigger: `roomler-agent self-update [--check-only]`.
- Release pipeline: `.github/workflows/release-agent.yml` builds signed MSI (cargo-wix), .deb (cargo-deb), and .pkg scaffolding on tag push; runs `encoder-smoke` on windows-latest as a smoke-test gate.

## Known Issues

- [CRITICAL] [2026-03-10] CORS is fully permissive — Status: FIXED (2026-03-21, uses configured cors_origins)
- [HIGH] [2026-03-10] No rate limiting — Status: FIXED (2026-03-21, tower_governor 60 req/min per IP)
- [HIGH] [2026-03-10] JWT default secret is "change-me-in-production" — must be overridden in prod — Status: OPEN
- [HIGH] [2026-04-17] Remote-control subsystem not yet live-tested end-to-end (agent → browser on a real display) — Status: FIXED (2026-04-18, verified on Win11 + openh264 against roomler.ai)
- [HIGH] [2026-04-18] Windows MF hardware encoder (NVENC / Intel QSV) is scaffolded but not yet functional — NVENC `ActivateObject` returns `0x8000FFFF` without a matching DXGI adapter; Intel QSV is async-only and ignores `MF_TRANSFORM_ASYNC_UNLOCK`; SW MFT fallback rejects LowDelayVBR and overshoots ~5× the target bitrate. Status: FIXED (2026-04-20, 0.1.26) — probe-and-rollback cascade lands the sync HW path; Auto prefers MF-HW on Windows with `ROOMLER_AGENT_HW_AUTO=0` escape hatch; Intel QSV async path still gated on commit 1A.2. Live-verified on RTX 5090 Laptop + AMD Radeon 610M.
- [MEDIUM] [2026-03-10] TypeScript type errors — Status: FIXED (2026-03-21, vue-tsc --noEmit passes)
- [MEDIUM] [2026-03-10] No security headers in nginx — Status: FIXED (2026-03-21, X-Frame-Options, X-Content-Type-Options, etc.)
- [MEDIUM] [2026-03-10] No CI pipeline — Status: FIXED (2026-03-21, GitHub Actions: clippy + build + test)
- [MEDIUM] [2026-04-17] Remote-control: clipboard + file-transfer data channels accepted on both sides but still log-only (no real handler) — Status: FIXED (2026-04-21, clipboard round-trip shipped in 0.1.32; file-transfer shipped in 0.1.33 — browser drag/pick → chunked upload over `files` DC with backpressure → write into host's Downloads folder, filename sanitization + collision-safe rename)
- [MEDIUM] [2026-04-17] Remote-control: consent auto-granted on agent (no tray UI yet); fine for self-controlled hosts, needs UI for org-controlled devices per docs §11.2 — Status: OPEN
- [LOW] [2026-03-10] Deployment strategy is Recreate (no zero-downtime rolling updates) — Status: OPEN
- [LOW] [2026-03-10] No git hooks configured (no pre-commit, no lint-staged) — Status: OPEN
- [LOW] [2026-04-17] Remote-control: encoder bitrate is fixed at 3 Mbps (TWCC/REMB adaptive bitrate is a no-op) — Status: FIXED (2026-04-20, 0.1.26 REMB-driven adaptive bitrate; openh264 set_bitrate via raw FFI; hysteresis ±15% prevents wobble)
- [LOW] [2026-04-17] Remote-control: agent captures primary display only; multi-monitor plumbing stops at the `mon` field in the wire protocol — Status: PARTIAL (2026-04-20, 0.1.31 — display enumeration now reports all attached monitors via `scrap::Display::all()`; capture backend still hardcodes `Display::primary()`, multi-monitor capture selection deferred)
- [LOW] [2026-04-20] Remote-control: NVIDIA NVENC `ActivateObject` returns 0x8000FFFF on RTX 5090 Blackwell for H.264, HEVC, and AV1 MFTs regardless of adapter binding. Cascade routes around it (H.264+HEVC land on alternative MFTs; AV1 has no alternative and fails cleanly, filtered from advertised caps by the probe-at-startup check). Worth a fresh investigation with driver updates or `CODECAPI_AVEncAdapterLUID` experiments. Status: OPEN (workaround shipped)
- [MEDIUM] [2026-04-22] Remote-control: Ctrl+C types `©` and Backspace types `^H` in pwsh / Windows Terminal on 0.1.33 and earlier. Root cause: `hid_to_key` mapped letters/digits to `Key::Unicode(c)`, which enigo routes through `KEYEVENTF_SCANCODE` on Windows — layout-sensitive path that mis-composed Ctrl + letter on non-US layouts. Status: FIXED (2026-04-22, 0.1.34 — letters/digits now route through `Key::Other(VK_*)` on Windows; Ctrl+C lands as VK_C with VK_CONTROL held so PSReadLine interprets it correctly).
- [MEDIUM] [2026-04-22] Remote-control: "Get remote clipboard → me" returned "clipboard worker gone" on the second call. Root cause: `Clipboard` is `Clone` (cheap Sender) but had a Drop impl that sent `ClipboardCmd::Shutdown` on every clone drop, killing the worker prematurely. Status: FIXED (2026-04-22, 0.1.34 — removed Drop impl; last-Sender-drops naturally ends the `rx.recv()` loop).
- [HIGH] [2026-04-22] Remote-control: 4K + HEVC HW on the hybrid RTX 5090 + Intel UHD 630 box caps at 7-8 fps even when dragging a window, because NVENC Blackwell fails (see 2026-04-20 entry) → cascade lands on Intel UHD 630 HEVC MFT which tops out at ~10-15 fps at 4K. Status: PARTIAL WORKAROUND (2026-04-22, 0.1.35) — new `rc:resolution` control lets the operator downscale the stream to 1080p/1440p where Intel UHD 630 HEVC sustains 30-60 fps; proper fix is GPU-side scale via `VideoProcessorMFT` (deferred, Tier 1C.3) or forcing the stream through H.264 SW openh264 when no strong HW path survives.
- [MEDIUM] [2026-04-22] Browser viewer: Chrome's `<video>` element enforces a ~80 ms jitter-buffer floor regardless of `jitterBufferTarget=0` / `playoutDelayHint=0`, which structurally caps how snappy the browser controller can feel next to a native viewer like RustDesk. Status: PARTIAL WORKAROUND (2026-04-22, 0.1.36, Tier B7) — new opt-in WebCodecs render path routes decoded frames straight to a canvas via `RTCRtpScriptTransform` + `VideoDecoder`, bypassing the jitter buffer. Chrome-only for v1 (Firefox uses a different insertable-streams API, Safari 17+ landing). Default off until we have enough field hours to flip it on by default.
- [HIGH] [2026-05-10] E2E first-cut in-cluster Playwright suite had ~60 hard fails on master (earlier handover misreported as "11" because the orchestrator's 30-min poll cap truncated the Playwright summary line out of view). Status: FIXED (2026-05-11) — **127 passed / 0 failed / 0 flaky** on run #24 (21.0 min). 18 commits closed all 60 + the rc.18 Tauri-tray CI breakage. Key wins: `ea2a619` (creator-self-join 409 closed ~25 fails in one go), `bd0492d` (disabled 6 Chromium feature-gates including `BlockInsecurePrivateNetworkRequests`, `HttpsUpgrades` to fix the ERR_ACCESS_DENIED 404 flakes that varied 3-6 fails per run), `e4c1314` (`TenantResponse.plan` was Debug-formatted `"Free"` but frontend compares to lowercase `"free"` plan ids from `/stripe/plans`). Full arc in `memory/project_e2e_cycle3_zero_fail.md`. Cycle 4 (Mailpit + oauth-mock + coturn + agent enrollment in `roomler-ai-e2e` ns to unblock 7 deferred specs) — Mailpit infra files shipped in `395fc07`; see `memory/project_next_session_cycle4_handover.md` for the 4 remaining chunks.

## Last Health Check

Date: (not yet run)
Result: N/A
Summary: Initial CLAUDE.md setup. First health check pending.

## Performance Baselines

(Populated after first health check run)
- Rust compilation time: TBD
- Test execution time: TBD
- Docker build time: TBD
- Binary size: TBD
- Docker image size: TBD

## Security Baseline

- Last CVE scan: not yet run
- JWT expiry: access=604800s (7 days), refresh=2592000s (30 days) (configurable via ROOMLER__JWT__*)
- Rate limit config: NONE
- CORS: PERMISSIVE (Any/Any/Any)
- nginx security headers: NONE
