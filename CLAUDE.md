# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**Roomler AI** is a real-time collaboration platform with chat, video conferencing, file sharing, and room management. Stack: Rust (Axum) + MongoDB + Vue 3/Vuetify 3 + Pinia + Mediasoup (WebRTC SFU).

## Commands

```bash
# Development
cargo run --bin roomler-ai-api           # Start backend (port 3000)
cd ui && bun run dev                   # Vite dev server (port 5000, proxies to 5001)
cd ui && bun run build                 # Production UI build (includes vue-tsc --noEmit)

# Testing
cargo test -p roomler-ai-tests           # All integration tests (114 tests, requires MongoDB+Redis)
cd ui && bun run test:unit             # Vitest unit tests
cd ui && bun run test:unit:coverage    # Vitest with coverage
cd ui && bun run e2e                   # Playwright E2E tests (18 spec files)

# Static Analysis
cargo clippy --workspace -- -D warnings   # Rust lint
cargo check --workspace                    # Rust compilation check
cd ui && vue-tsc --noEmit                  # Vue TypeScript check

# Dependency Audit
cargo audit                            # Rust CVE scan (requires cargo-audit)
cargo outdated                         # Rust outdated deps (requires cargo-outdated)
cd ui && bun audit                     # JS/TS vulnerability scan
cd ui && bun outdated                  # JS/TS outdated deps

# Infrastructure
docker compose up -d                   # Start MongoDB (27019), Redis (6379), MinIO (9000), coturn
```

## Architecture

```
crates/
  config/    → Settings (env vars via ROOMLER__ prefix, config crate)
  db/        → MongoDB models (19 models) + indexes (15 collections) + native driver v3.2
  services/  → Business logic: auth, DAOs, media (mediasoup), export, background tasks, OAuth, push, email, Stripe, Giphy, Claude AI
  api/       → Axum HTTP/WS server: ~75 API routes + /ws + /health
  tests/     → Integration tests (14 test modules, 114 tests)
ui/
  src/
    api/           → HTTP client (client.ts)
    components/    → Vue components (20 files in 6 categories)
    composables/   → 10 custom hooks (useAuth, useWebSocket, useMarkdown, etc.)
    stores/        → 12 Pinia stores (setup store pattern)
    views/         → 13 view modules (auth, chat, conference, dashboard, files, rooms, etc.)
    plugins/       → router, pinia, vuetify, i18n
```

### Crate dependency flow
`config` <- `db` <- `services` <- `api`
`tests` depends on `api` + `config` + `db` (spawns real servers with random ports and test databases)

## Multi-Tenancy

All data is scoped by `tenant_id`. Routes are nested: `/api/tenant/{tenant_id}/room/{room_id}/message/...`. The `tenant_members` collection tracks user-tenant membership. Room membership is tracked via `room_members`.

## Auth Pattern

JWT-based auth (jsonwebtoken 9 crate) with Argon2 password hashing:
- Access token: configurable TTL (default 3600s)
- Refresh token: configurable TTL (default 604800s)
- Auth middleware extracts user from `Authorization: Bearer` header
- OAuth: Google, Facebook, GitHub, LinkedIn, Microsoft

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

Route groups: auth (7), user (2), oauth (2), stripe (4), invite (2+4), giphy (2), push (3), notification (5), tenant (3), member (2), role (6), room (16), message (11), recording (3), file (7), task (3), export (2), search (1), health (1), ws (1).

## DB Model Pattern

MongoDB native driver (not Mongoose). Models in `crates/db/src/models/`:
- 15 collections: tenants, users, tenant_members, roles, rooms, room_members, messages, reactions, recordings, files, invites, background_tasks, audit_logs, notifications, custom_emojis, activation_codes
- Indexes defined in `crates/db/src/indexes.rs` (unique, TTL, text indexes on email, username, slug, code, content, etc.)
- Text indexes on messages (content), rooms (name, purpose, tags), users (display_name, username) for full-text search
- TTL indexes on audit_logs (90 days), activation_codes, background_tasks
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
- Test modules: auth, tenant, room, message, reaction, recording, file, invite, role, notification, push, giphy, oauth, call, pagination, rate_limit, cors

**E2E tests** (`ui/e2e/`):
- Playwright 1.58 with Chromium (fake media stream devices for WebRTC)
- 24 spec files: auth, channels, chat, chat-multi, chat-pagination, chat-reactions, chat-threads, conference (4 specs), connection-status, dashboard, files, invite, mention, notifications, oauth, profile, room-fixes, room-management, websocket, 404
- Fixtures in `ui/e2e/fixtures/test-helpers.ts`
- Base URL: `http://localhost:5000` (or E2E_BASE_URL env var)

**Unit tests** (`ui/src/`):
- Vitest with jsdom environment, 215 tests across 13 files
- Stores: auth, messages, rooms, ws, notifications, conference, tenants, files
- Composables: useValidation, useSnackbar, useMarkdown
- API client: token injection, error handling
- Plugins: vuetify theme config

## Environment

- `.env` — development (not committed, in .gitignore)
- Config via `ROOMLER__` prefixed env vars (double underscore separator)
- Docker: `docker-compose.yml` runs MongoDB 7 (auth: roomler/R00m1eR_5uper5ecretPa55word), Redis 7, MinIO, coturn
- Default DB URL: `mongodb://localhost:27019` (tests use no auth)

## Deployment

- **Docker**: Multi-stage build (rust:1.88-bookworm -> oven/bun:1 -> debian:trixie-slim + nginx)
- **Deploy repo**: `/home/gjovanov/roomler-ai-deploy/` (Ansible + K8s)
- **Pipeline**: `docker build` -> `docker save` -> `scp` to k8s-worker-3 -> `ctr import` -> Ansible playbook
- **K8s**: Namespace `roomler-ai`, deployment `roomler-ai`, Recreate strategy, hostNetwork
- **Health probes**: startup/readiness/liveness all on `/health` (port 80 via nginx -> :3000 backend)
- **nginx**: Pod-internal reverse proxy (`files/nginx-pod.conf`) — SPA fallback + API proxy + WS proxy

## Post-Implementation Testing

After every feature or fix, verify your changes:

| Change type | Command | What it checks |
|-------------|---------|----------------|
| Backend (models, services, routes) | `cargo test -p roomler-ai-tests` | Integration tests (real MongoDB) |
| Frontend (views, stores, composables) | `cd ui && bun run build` | TypeScript + Vite build |
| Full-flow (auth, routes, UI+API) | `cd ui && bun run e2e` | Playwright E2E tests |

Run the **most specific** command first. If a backend change also affects the frontend, run both.

## Known Issues

- [CRITICAL] [2026-03-10] CORS is fully permissive — Status: FIXED (2026-03-21, uses configured cors_origins)
- [HIGH] [2026-03-10] No rate limiting — Status: FIXED (2026-03-21, tower_governor 60 req/min per IP)
- [HIGH] [2026-03-10] JWT default secret is "change-me-in-production" — must be overridden in prod — Status: OPEN
- [MEDIUM] [2026-03-10] TypeScript type errors — Status: FIXED (2026-03-21, vue-tsc --noEmit passes)
- [MEDIUM] [2026-03-10] No security headers in nginx — Status: FIXED (2026-03-21, X-Frame-Options, X-Content-Type-Options, etc.)
- [MEDIUM] [2026-03-10] No CI pipeline — Status: FIXED (2026-03-21, GitHub Actions: clippy + build + test)
- [LOW] [2026-03-10] Deployment strategy is Recreate (no zero-downtime rolling updates) — Status: OPEN
- [LOW] [2026-03-10] No git hooks configured (no pre-commit, no lint-staged) — Status: OPEN

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
