# Testing

Roomler2 has three test layers: Rust integration tests (114 tests), Vitest component unit tests, and Playwright E2E tests.

## Integration Tests

Located in `crates/tests/src/`. These tests spin up the full Axum server and interact with it via HTTP using `reqwest`.

### Test Modules

| File | Coverage Area |
|------|--------------|
| `auth_tests.rs` | Registration, login, logout, refresh, /me |
| `channel_tests.rs` | Room join, leave, list, explore |
| `channel_crud_tests.rs` | Room create, update, delete |
| `message_tests.rs` | Send, edit, delete, list, pin, threads + WS broadcast sender exclusion |
| `reaction_tests.rs` | Add and remove reactions |
| `conference_tests.rs` | Room calls: start, join, leave, end + mediasoup signaling (WS media:join, transport creation, peer_left broadcast) + connection_id isolation |
| `conference_message_tests.rs` | In-call chat messages: create, list, WS broadcast |
| `recording_tests.rs` | Create, list, delete recordings |
| `transcription_tests.rs` | Create, list, get transcriptions |
| `file_tests.rs` | Upload, get, download, delete, list files |
| `export_tests.rs` | Conversation export to XLSX |
| `pdf_export_tests.rs` | Conversation export to PDF |
| `multi_tenancy_tests.rs` | Cross-tenant data isolation |
| `invite_tests.rs` | Invite creation, acceptance, listing, revocation |
| `oauth_tests.rs` | OAuth provider linking |

### Test Fixtures

| File | Purpose |
|------|---------|
| `fixtures/test_app.rs` | Starts a test server on a random port, provides a configured `reqwest::Client` |
| `fixtures/seed.rs` | Creates test users, tenants, rooms, and messages for test setup |

### Running Integration Tests

```bash
# Run all integration tests
cargo test -p roomler2-tests

# Run with race detection (recommended)
cargo test -p roomler2-tests -- --test-threads=1

# Run a specific test module
cargo test -p roomler2-tests auth_tests

# Run with output
cargo test -p roomler2-tests -- --nocapture
```

Integration tests require a running MongoDB instance (see `docker-compose.yml`).

## Vitest Unit Tests

Located in `ui/src/**/__tests__/`. Component and config unit tests using Vitest + @vue/test-utils + jsdom.

### Configuration

- Config file: `ui/vitest.config.ts`
- Environment: jsdom
- Coverage: v8

### Test Modules

| File | Coverage Area |
|------|--------------|
| `plugins/__tests__/vuetify.spec.ts` | Theme config (light/dark colors, default theme, localStorage) |

### Running Unit Tests

```bash
cd ui

# Run all unit tests
bun run test:unit

# Run with watch mode
bun run test:unit:watch

# Run with coverage report
bun run test:unit:coverage
```

## E2E Tests

Located in `ui/e2e/`. Playwright tests that run against the full stack (backend + frontend).

### Test Specs

| File | Coverage Area |
|------|--------------|
| `auth.spec.ts` | Login and registration flows |
| `dashboard.spec.ts` | Dashboard rendering, tenant cards |
| `channels.spec.ts` | Room creation, browsing |
| `chat.spec.ts` | Message sending, display |
| `chat-multi.spec.ts` | Multi-participant chat: 4 users, message dedup (no duplicates from WS broadcast) |
| `conference.spec.ts` | Call view, join/leave, local video, mute/camera toggles |
| `conference-chat.spec.ts` | In-call chat functionality |
| `conference-list.spec.ts` | Room listing page, create dialog, navigation |
| `conference-multi.spec.ts` | Multi-participant calls: 2 users in separate browser contexts |
| `files.spec.ts` | File upload, browsing |
| `invite.spec.ts` | Invite creation, acceptance flows |
| `oauth.spec.ts` | OAuth redirect and callback |
| `room-fixes.spec.ts` | Dashboard Start Call, Chat View call button, child rooms, call notifications |
| `transcription.spec.ts` | Transcription display and controls |
| `websocket.spec.ts` | WebSocket connection, typing indicators |

### Test Helpers

| File | Purpose |
|------|---------|
| `fixtures/test-helpers.ts` | Login helper, page setup, API utilities (register, createTenant, createRoom, joinRoom, sendMessage, startCall, endCall, addTenantMember) |

### Running E2E Tests

```bash
cd ui

# Run all E2E tests
bun run e2e

# Run with Playwright UI
bun run e2e:ui

# Run a specific spec
npx playwright test e2e/auth.spec.ts

# Run in headed mode
npx playwright test --headed
```

E2E tests require both the backend (`cargo run`) and frontend (`bun run dev`) to be running.

## Test Architecture

```
Integration Tests (Rust)    Unit Tests (Vitest)      E2E Tests (Playwright)
┌─────────────────────┐    ┌───────────────────┐    ┌─────────────────────┐
│ reqwest HTTP Client  │    │ jsdom Environment  │    │ Chromium Browser     │
│         │            │    │        │           │    │        │             │
│         ▼            │    │        ▼           │    │        ▼             │
│  Axum Test Server    │    │ Vue Test Utils +   │    │  Vue 3 SPA (:5000)  │
│  (random port)       │    │ Component Mounts   │    │        │             │
│         │            │    │                    │    │        ▼             │
│         ▼            │    └───────────────────┘    │  Axum API (:5001)    │
│  Mongo (test DB)     │                             │        │             │
│                      │                             │        ▼             │
└─────────────────────┘                             │  Mongo + Redis +     │
                                                     │  MinIO               │
                                                     └─────────────────────┘
```

Integration tests use an isolated test database and server instance per test. Unit tests run in jsdom with mocked dependencies. E2E tests run against the development stack.

## Conference Stress Test

A Node.js stress test script that measures the maximum number of participants that can join a single mediasoup video conference. Located at `stress-test-conference.mjs`.

### What It Tests

Each simulated participant goes through the full signaling lifecycle:

1. **Register** user via REST API
2. **Add as tenant member** via direct MongoDB insert
3. **REST join** conference (adds participant to DB)
4. **WebSocket connect** and authenticate
5. **`media:join`** signaling — receives `router_capabilities` + `transport_created`
6. **`media:connect_transport`** — connects both send and recv transports with DTLS parameters

Participants are added in configurable batches (default: 10 per batch) with latency measured at each phase. The test stops when the failure rate exceeds 30% of a batch or the participant limit is reached.

### Running

```bash
# Prerequisites: API server running on :5001, MongoDB on :27019
npm install ws mongodb   # one-time, from project root

# Run with defaults (500 max, batches of 10)
node stress-test-conference.mjs

# Results are written to stress-test-results.txt
```

### Configuration

Environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `API_URL` | `http://localhost:5001` | API base URL |
| `WS_URL` | `ws://localhost:5001/ws` | WebSocket base URL |
| `MONGO_URL` | `mongodb://localhost:27019` | MongoDB connection |
| `DB_NAME` | `roomler2` | Database name |
| `RESULTS_FILE` | `stress-test-results.txt` | Output file path |

Script constants (edit in file):

| Constant | Default | Description |
|----------|---------|-------------|
| `BATCH_SIZE` | 10 | Participants added per batch |
| `MAX_PARTICIPANTS` | 500 | Stop after this many join |
| `SIGNALING_TIMEOUT_MS` | 15000 | Timeout for WS responses |
| `FAILURE_RATE_THRESHOLD` | 0.3 | Stop if >30% of batch fails |
| `SETTLE_MS` | 500 | Pause between batches |

### Metrics Collected

Per batch:
- **Avg / P50 / P95 join latency** (full lifecycle: register → transport connect)
- **API process RSS** (memory)
- **System load average**
- **Success / failure count**

Final report:
- Max participants joined
- Memory growth (RSS start → end)
- Latency trend table
- Stop reason and failure details

### Benchmark Results

#### Hardware: AMD Ryzen 9 9955HX3D (WSL2)

| Spec | Value |
|------|-------|
| CPU | AMD Ryzen 9 9955HX3D, 16 cores / 32 threads |
| RAM | 47 GB |
| OS | Linux 6.6.87 (WSL2) |
| Build | Debug (unoptimized) |
| mediasoup workers | 2 |

#### Results: 500 Participants, 0 Failures

```
Participants | Avg Latency | P95 Latency | API RSS
─────────────┼─────────────┼─────────────┼────────
          10 |      487ms  |      512ms  |  746MB
         100 |      473ms  |      509ms  |  832MB
         200 |      492ms  |      557ms  |  902MB
         300 |      469ms  |      490ms  |  950MB
         400 |      478ms  |      533ms  |  945MB
         500 |      469ms  |      490ms  | 1085MB
```

| Metric | Value |
|--------|-------|
| Max participants (no failures) | **500** (test limit reached) |
| Avg join latency | **~480ms** (flat, no degradation) |
| P95 join latency | **~530ms** (stable) |
| Memory per participant | **~0.84 MB** |
| API RSS growth | 667MB → 1085MB (+418MB) |
| System load at 500 | 2.51 (light) |
| System memory at 500 | 9.5GB / 47GB (20%) |

Key observations:
- **No latency degradation** from participant 1 to 500 — join time stayed flat at ~480ms
- **Linear memory growth** at ~0.84MB per participant (2 WebRTC transports each)
- **No failure point found** — the system had 80% memory headroom at 500 participants
- **Projected capacity** on this hardware: ~2000-4000 participants based on memory growth rate (with release build and more mediasoup workers, likely higher)
- The ~480ms latency is dominated by user registration + MongoDB inserts, not mediasoup signaling
