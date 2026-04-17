# Architecture

## System Overview

```
┌──────────────┐         ┌───────────────────────────────────┐
│  Browser SPA │  HTTP   │         Axum API :5001             │
│  Vue 3       ├────────►│                                   │
│  :5000       │  WS     │  ┌─────────┐  ┌───────────────┐  │
│              ◄────────►│  │ REST    │  │  WebSocket    │  │
└──────────────┘         │  │ Routes  │  │  Handler      │  │
                         │  └────┬────┘  └───────┬───────┘  │
                         │       │               │          │
                         │  ┌────▼───────────────▼───────┐  │
                         │  │      Services Layer        │  │
                         │  │  auth / dao / export /     │  │
                         │  │  cloud_storage / media     │  │
                         │  └────────────┬───────────────┘  │
                         │               │                  │
                         └───────────────┼──────────────────┘
                                         │
              ┌──────────────────────────┼──────────────────────────┐
              │                          │                          │
       ┌──────▼──────┐          ┌───────▼───────┐         ┌───────▼───────┐
       │  MongoDB 7  │          │   Redis 7     │         │    MinIO      │
       │  :27019     │          │   :6379       │         │  :9000/:9001  │
       └─────────────┘          └───────────────┘         └───────────────┘
```

## Cargo Workspace

The project is organized as a Rust workspace with 5 crates:

```
roomler-ai/
├── crates/config     # Configuration loading
├── crates/db         # Models, DAOs, indexes
├── crates/services   # Business logic
├── crates/api        # HTTP + WebSocket layer
└── crates/tests      # Integration tests
```

### Crate Responsibilities

| Crate | Purpose | Key Dependencies |
|-------|---------|-----------------|
| `config` | Load settings from config files + `ROOMLER__` env vars | `config`, `serde` |
| `db` | Define 18 MongoDB models, indexes, base DAO trait | `mongodb`, `bson`, `serde` |
| `services` | Auth (JWT + argon2), DAOs, export, cloud storage, mediasoup SFU | `jsonwebtoken`, `argon2`, `rust_xlsxwriter`, `mediasoup` |
| `api` | Axum router, REST routes, WebSocket handler, middleware | `axum`, `tower-http` |
| `tests` | Integration test suite (15 test modules + fixtures) | `reqwest`, `tokio-test` |

### Dependency Graph

```
tests ──► api ──► services ──► db ──► config
```

Each crate depends only on the crates to its right. `tests` depends on `api` to spin up the full server for integration testing.

## Request Flow

```
Browser
  │
  ├─► HTTP Request
  │     │
  │     ▼
  │   Axum Router (/api/...)
  │     │
  │     ▼
  │   CORS + Trace Middleware (tower-http)
  │     │
  │     ▼
  │   Auth Extractor (JWT from cookie or header)
  │     │
  │     ▼
  │   Route Handler (routes/*.rs)
  │     │
  │     ▼
  │   Service / DAO Layer
  │     │
  │     ▼
  │   MongoDB Driver
  │
  └─► WebSocket Upgrade (/ws?token=JWT)
        │
        ▼
      JWT Verification
        │
        ▼
      WsStorage (register connection)
        │
        ▼
      Message Loop (ping/pong, typing, presence, media signaling)
        │
        ├─► Dispatcher (broadcast to channel members)
        │
        └─► mediasoup Signaling
              │
              ▼
            WorkerPool → Router → WebRtcTransport → Producer/Consumer
```

## Backend Layers

### API Layer (`crates/api`)

- **Routes** -- REST endpoint handlers organized by domain (auth, tenant, room, message, file, recording, etc.)
- **WebSocket** -- Connection management (`WsStorage`), message dispatch, presence and typing indicators
- **Middleware** -- Authentication via `AuthUser` extractor (JWT from httpOnly cookie or Authorization header)
- **Error Handling** -- Unified `ApiError` type maps to HTTP status codes

### Service Layer (`crates/services`)

- **Auth** -- JWT token generation/verification, argon2 password hashing
- **DAOs** -- Data access objects for each model (CRUD + domain queries)
- **Export** -- Conversation export to XLSX (`rust_xlsxwriter`) and PDF (`genpdf`)
- **Cloud Storage** -- S3/MinIO file operations
- **Background Tasks** -- Async processing for recordings, exports
- **Media** -- mediasoup 0.20 SFU: WorkerPool (round-robin), RoomManager (Router/Transport/Producer/Consumer), WebSocket signaling protocol

### Data Layer (`crates/db`)

- **Models** -- 18 Rust structs with `serde` Serialize/Deserialize
- **Indexes** -- Unique and compound indexes for all collections
- **Base DAO** -- Generic CRUD trait for MongoDB operations

## Frontend Layers

```
Views (pages)
  │
  ▼
Components (reusable UI)
  │
  ▼
Pinia Stores (12 stores: auth, tenant, rooms, messages, files, invite, notification, role, user, tasks, ws)
  │
  ▼
Composables (useAuth, useWebSocket)
  │
  ▼
API Client (REST + WebSocket)
  │
  ▼
Axum Backend (HTTP :5001 / WS :5001)
```

### Frontend Stack

- **Vue 3** with Composition API
- **Vuetify 3** for Material Design components
- **Pinia** for state management
- **Vue Router** with auth guards
- **vue-i18n** for internationalization
- **Vite 7** for build tooling
- **Playwright** for E2E testing
