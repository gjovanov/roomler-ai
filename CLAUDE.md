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
- Access token: configurable TTL (default 3600s)
- Refresh token: configurable TTL (default 604800s)
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
- **GitOps**: ArgoCD at [argocd.roomler.ai](https://argocd.roomler.ai) reconciles the `roomler-ai` Application from `github.com/gjovanov/roomler-ai-deploy @ master` path `k8s/overlays/prod`. Sync policy is **Manual** — a `git push` won't auto-deploy; run `argocd app sync roomler-ai --grpc-web` (or click Sync in the UI) when ready. Verify the live targetRevision with `argocd app get roomler-ai --grpc-web | grep -E "Target|Sync Status"`.
- **Image registry**: `registry.roomler.ai` (self-hosted Docker Registry v2 on mars, basic auth, cert auto-renewed via acme.sh). Pull secret `regcred` lives in the `roomler-ai` namespace.
- **K8s cluster**: 3 control-plane + 3 worker nodes (Ubuntu 22.04, containerd 1.7.29, v1.31.14). Pods run on `k8s-worker-3` (10.10.30.11). Namespace `roomler-ai`, deployment `roomler2` (note: name is `roomler2` not `roomler-ai`), Recreate strategy, hostNetwork, `imagePullPolicy: IfNotPresent`.
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

**Resumption note after a session break**: `HANDOVER9.md` is the most recent (covers Tier A + viewer controls + in-field fixes in 0.1.33 → 0.1.35). `HANDOVER2.md` through `HANDOVER8.md` trace the historical arc — read `HANDOVER9.md` first for current state.

**Wire protocol**: `rc:*` JSON messages over the existing `/ws` endpoint. `ClientMsg` / `ServerMsg` in `crates/remote_control/src/signaling.rs`. ObjectIds are raw hex strings (locked by tests); `Permissions` serialises as pipe-separated names (bitflags 2.x convention, also locked).

**WebSocket role multiplexing**: `/ws?token=<jwt>&role=agent` uses the agent JWT audience; no `role` param (or `role=user`) uses the existing user flow. Same WS endpoint, same handshake, different claim validator.

**Status at 0.1.36**:
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
- JWT expiry: access=3600s, refresh=604800s (configurable via ROOMLER__JWT__*)
- Rate limit config: NONE
- CORS: PERMISSIVE (Any/Any/Any)
- nginx security headers: NONE
