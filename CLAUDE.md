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

**Resumption note after a session break**: `HANDOVER22.md` is the most recent — captures the rc.28 install-wizard ship state and the W10 manual-smoke checklist. Earlier HANDOVERs (`HANDOVER2.md` → `HANDOVER21.md`) trace the historical arc.

**Wire protocol**: `rc:*` JSON messages over the existing `/ws` endpoint. `ClientMsg` / `ServerMsg` in `crates/remote_control/src/signaling.rs`. ObjectIds are raw hex strings (locked by tests); `Permissions` serialises as pipe-separated names (bitflags 2.x convention, also locked).

**WebSocket role multiplexing**: `/ws?token=<jwt>&role=agent` uses the agent JWT audience; no `role` param (or `role=user`) uses the existing user flow. Same WS endpoint, same handshake, different claim validator.

**Status at 0.3.0-rc.27 + rc.28** (Tauri 2 install + onboarding wizard, code in master 2026-05-15 — W10 manual smoke pending before tag):
- **rc.27 foundational lib** ships independent of the wizard: registry probe for existing installs (`install_detect::detect_existing_install` reads HKCU + HKLM `Software\…\Installer\UpgradeCodes\<packed>`), Windows Installer compressed-GUID encoder + WiX UpgradeCode parity tests (`install_detect::{pack,unpack}_msi_guid`), JWT introspection that NEVER echoes the raw token (`jwt_introspect::parse_unverified`), SCM Environment-block REG_MULTI_SZ R/M/W helpers + `restart_service` (`win_service::environment::*`), and 5 visibility lifts in `updater.rs` so the wizard crate can reuse the existing UAC-elevated msiexec spawn path. Backend `/api/agent/installer/{flavour}` + `/health` proxy in `crates/api/src/routes/agent_release.rs` streams MSI bytes through `roomler.ai` (NOT `github.com`) so corporate ESET / Defender allow-lists trust the download.
- **rc.28 wizard EXE** is a new `agents/roomler-installer/` workspace member. Tauri 2 single-window app (NO tray icon) that the operator double-clicks to walk through 5 wizard steps (Welcome / Server / Token / Install / Done). Replaces the rc.18-era manual ritual: pick the right MSI flavour → run msiexec → elevated PowerShell for `service install --as-service` → elevated `reg add` for the SystemContext env var → CLI `roomler-agent enroll`. The wizard owns every step + surfaces inline UI per failure mode.
- **Lib name `wizard_core` (NOT `roomler_installer`)** — Windows UAC's "installer detection" filename heuristic auto-elevates any 32/64-bit EXE whose name contains "install" / "setup" / "update" / "patch". Cargo derives test-binary names from the lib crate, so a lib named `roomler_installer` produced `roomler_installer-<hash>.exe` which UAC refused to launch from a non-elevated `cargo test`. Renamed lib only; user-facing bin EXE keeps the `roomler-installer` marketing name.
- **SystemContext flavour caveat (v1 limit)** — `permachine-system-context` runs the perMachine MSI then surfaces a PowerShell snippet on the Done page (`roomler-agent set-service-env-var --name ROOMLER_AGENT_ENABLE_SYSTEM_SWAP --value 1 && roomler-agent restart-service`). Full automatic SCM env-var write + service restart needs a clean self-elevation path that doesn't surface a second UAC prompt mid-flow; deferred to **rc.29**.
- **Authenticode signing in `release-agent.yml`** — the wizard EXE MUST be signed for production (BLOCKER-10 fix from the plan critique: corporate AV would quarantine an unsigned downloader as a "downloader trojan"). The Windows job's signing step now signs all 4 artifacts (perUser MSI + perMachine MSI + tray EXE + installer EXE) in one signtool call. Tray-signing is the bonus side-effect — pre-rc.28 the tray shipped unsigned.
- **Cancel + force-kill (H4)** — pre-spawn `CANCEL_REQUESTED: AtomicBool` flips on `cmd_cancel_in_progress`; orchestrator checks at each await point. Post-spawn `ACTIVE_MSI_PID: AtomicU32` lets `cmd_force_kill_msi` call `TerminateProcess` on the running msiexec. SPA renders a confirmation dialog before exposing the force-kill button (the operator gets "may leave partial install" warning).
- **Progress streaming (H1)** — `cmd_install` emits a Tauri 2 `ipc::Channel<ProgressEvent>` stream (17 event variants) + mirrors every emit into an in-Rust `ProgressLog` so a late-attaching SPA listener catches up via `cmd_install_progress_replay`. `DownloadProgress` events collapse into a single progress bar to avoid log bloat (only the final `DownloadVerified` event makes it into the replay log for downloads).
- **State persistence (H5)** — wizard step + form fields persist to `%LOCALAPPDATA%\roomler\roomler-installer\wizard-state.json` so a force-killed wizard resumes mid-flow. **Token is NEVER persisted** — if the operator killed the wizard mid-flow with a token already pasted, the resume drops them on the Token step asking to paste again. Tests assert wire shape + that missing-file / corrupt-JSON / schema-version mismatch all return Default cleanly.
- **Cross-flavour switch UI gate (BLOCKER-7)** — Welcome step shows a yellow banner when the detected install is the opposite flavour of what the operator picked, with an "I understand my enrollment will be lost; I have a fresh token" ack checkbox gating the Continue button. machine_id is preserved for same-flavour upgrades (`derive_machine_id` is deterministic over `(hostname + os + arch + config_path)`); cross-flavour shifts config_path so machine_id changes — operator needs a fresh enrollment token.
- **Tests**: 42 installer-lib unit tests including a live Win32 smoke that spawns `cmd /c exit 1602`, attaches `MsiRunner` via `OpenProcess` + `WaitForSingleObject` + `GetExitCodeProcess`, asserts `MsiExitDecoded::UserCancel`. 328 agent-lib tests (+40 net new from rc.27). Backend has 11 new unit tests on the installer-proxy helpers. Clippy + fmt clean.
- **Local release build verified** — `cargo build -p roomler-installer --release` produces `roomler-installer.exe` (14.4 MB) on Win11 MSVC. End-to-end Tauri runtime + custom-protocol bundled assets work.
- **Pending W10 manual smoke** — wizard has NEVER been launched end-to-end on a fresh Win11 VM. Smoke pack S1-S10 (10 scenarios incl. S3a same-flavour / S3b cross-flavour / S5a pre-spawn-cancel / S5b post-spawn-force-kill / S9 single-instance-mid-install) covers the failure modes the plan critique flagged. Once smoke green → tag `agent-v0.3.0-rc.28`. **See `HANDOVER22.md` for the full W10 checklist.**

**Older releases (0.1.x → 0.3.0-rc.26)**: CLAUDE.md no longer mirrors per-release notes — `HANDOVER2.md` → `HANDOVER22.md`, `docs/remote-control.md`, and `git log` are authoritative for the historical arc. Key milestones, all shipped: live-verified WebRTC P2P (0.1.36), MF H.264 HW cascade with REMB-driven bitrate (0.1.26), codec negotiation H.264/HEVC/AV1 with probe-at-startup filter (0.1.28-0.1.30), clipboard + file-transfer data channels (0.1.32-0.1.33), WebCodecs canvas render bypass (0.1.36, Tier B7), agent lifecycle service hooks + auto-update (0.1.36), failure-resilience cycle with watchdog + crash rollback + SHA256 verification (0.1.50-0.1.54), heartbeat telemetry + pre-flight checks + opt-in Windows Service mode (0.1.55-0.1.58), M5 verification + clean-exit fixes + install-storm cooldown (0.1.61-0.1.63), M3 Z-path lock-screen overlay + browser auto-reconnect + perMachine MSI (0.2.0-0.2.5; A1 WGC NO-GO empirically confirmed), auto-update asset-picker flavour-aware (0.2.6), input regression fix (0.2.7), M3 A1 SystemContext-from-cold-start (rc.1-rc.7), UAC self-update + cross-flavour MSI cleanup + Tauri tray companion (rc.18), resumable file-DC transfers (rc.19-rc.20), ESET-evasive PROGRAMDATA staging (rc.21-rc.22), SystemContext Winlogon + elevated apps gating via `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP` (rc.26).

## Known Issues (OPEN only)

Fixed-and-shipped issues live in `git log` / the HANDOVERs. Currently open:

- [HIGH] [2026-03-10] JWT default secret is "change-me-in-production" — must be overridden in prod.
- [MEDIUM] [2026-04-17] Remote-control: consent auto-granted on agent (no tray-driven prompt yet); fine for self-controlled hosts, needs UI for org-controlled devices per docs §11.2.
- [LOW] [2026-03-10] Deployment strategy is Recreate (no zero-downtime rolling updates).
- [LOW] [2026-03-10] No git hooks configured (no pre-commit, no lint-staged).
- [LOW] [2026-04-20] Remote-control: NVIDIA NVENC `ActivateObject` returns 0x8000FFFF on RTX 5090 Blackwell for H.264 / HEVC / AV1 MFTs regardless of adapter binding. Cascade routes around it (H.264+HEVC land on alternative MFTs; AV1 has no alternative and is filtered from advertised caps by the probe-at-startup check). Worth re-testing on newer drivers / `CODECAPI_AVEncAdapterLUID` experiments.
- [MEDIUM] [2026-04-22] Browser viewer: Chrome's `<video>` enforces a ~80 ms jitter-buffer floor regardless of `jitterBufferTarget=0` / `playoutDelayHint=0`. Partial workaround shipped (opt-in WebCodecs canvas render path, Chrome-only) — flip on by default once field hours accumulate.

## Security Baseline

- JWT expiry: access=604800s (7 days), refresh=2592000s (30 days) (configurable via ROOMLER__JWT__*)
- Rate limiting: tower_governor 60 req/min per IP (2026-03-21)
- CORS: configured via `cors_origins` (2026-03-21; no longer permissive)
- nginx security headers: X-Frame-Options, X-Content-Type-Options, etc. (2026-03-21)
- Last CVE scan: not yet run
