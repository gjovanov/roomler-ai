# Roomler2

Real-time communication and collaboration platform with multi-tenancy, hierarchical rooms (text + voice/video), chat, video conferencing (mediasoup WebRTC SFU), file sharing, cloud storage integrations, and AI-powered document recognition.

## Features

| Category | Feature | Status |
|----------|---------|--------|
| **Multi-Tenancy** | Organizations with plans (Free/Pro/Business/Enterprise) | :white_check_mark: |
| | Roles & permissions (Owner, Admin, Moderator, Member) with 24-bit bitfield | :white_check_mark: |
| | Role CRUD, assignment, default role seeding | :white_check_mark: |
| | Invite system (shareable links, email invites, batch invites) | :white_check_mark: |
| | User profiles (bio, avatar, presence, timezone, locale) | :white_check_mark: |
| | OAuth login (Google, Facebook, GitHub, LinkedIn, Microsoft) | :white_check_mark: |
| **Rooms** | Unified rooms with hierarchy (text + voice/video) | :white_check_mark: |
| | Parent/child room tree, explore & member management | :white_check_mark: |
| | In-room call start/join with real-time notifications | :white_check_mark: |
| **Real-Time Chat** | Threaded messages with replies | :white_check_mark: |
| | Reactions (unicode + custom emoji) | :white_check_mark: |
| | @mentions with autocomplete (TipTap) | :white_check_mark: |
| | Embeds, attachments, pinning | :white_check_mark: |
| | Typing indicators & presence | :white_check_mark: |
| **Notifications** | @mention notifications (real-time via WebSocket) | :white_check_mark: |
| | Notification bell with unread count badge | :white_check_mark: |
| | Mark read / mark all read | :white_check_mark: |
| **Video Conferencing** | mediasoup WebRTC SFU (per-room calls) | :white_check_mark: |
| | Start/join calls from room chat or dashboard | :white_check_mark: |
| | Multiple producers/consumers per participant | :white_check_mark: |
| | Recordings, transcriptions & in-call chat | :white_check_mark: |
| **File Management** | Versioned uploads, multipart upload | :white_check_mark: |
| | Cloud sync (Google Drive, OneDrive, Dropbox) | :white_check_mark: |
| | AI document recognition (Claude API) | :white_check_mark: |
| **Export** | Conversation export to XLSX | :white_check_mark: |
| | PDF export (custom raw PDF 1.4 generator) | :white_check_mark: |
| | Background task processing | :white_check_mark: |
| **WebSocket** | Live presence & typing indicators | :white_check_mark: |
| | Real-time message delivery | :white_check_mark: |
| | Media signaling (join/produce/consume) | :white_check_mark: |
| **Frontend** | Vue 3 + Vuetify 3 SPA | :white_check_mark: |
| | 12 Pinia stores, 19 views | :white_check_mark: |
| | Light/Dark theme toggle with localStorage persistence | :white_check_mark: |
| | mediasoup-client composable + VideoTile | :white_check_mark: |
| | Vitest unit tests + Playwright E2E | :white_check_mark: |

## Tech Stack

| Layer | Technology |
|-------|-----------|
| **Backend** | Rust (edition 2024), Axum 0.8, Tokio |
| **Frontend** | Vue 3, Vuetify 3, Pinia, Vue Router, vue-i18n |
| **Database** | MongoDB 7 (mongodb 3.2 driver) |
| **WebRTC** | mediasoup 0.20 (SFU), mediasoup-client 3.7 |
| **Auth** | JWT (argon2 hashing), httpOnly cookies, OAuth 2.0 |
| **AI** | Claude API (document recognition) |
| **Cloud** | Google Drive, OneDrive, Dropbox (OAuth2 + async_trait) |
| **Testing** | 114 Rust integration tests, 15 Playwright E2E specs, Vitest unit tests |
| **Infrastructure** | Docker Compose (MongoDB, Redis, MinIO, Coturn) |

## Architecture

```mermaid
graph TB
    subgraph Client["Browser"]
        UI["Vue 3 + Vuetify 3 SPA<br/>Pinia (12 stores) · mediasoup-client 3.7<br/>:5000"]
    end

    subgraph Server["Rust / Axum 0.8"]
        REST["REST Routes<br/>18 modules"]
        WS["WebSocket Handler<br/>Presence · Typing · Signaling"]
        AUTH["Auth Middleware<br/>JWT · OAuth 2.0 · argon2"]
        SVC["Services Layer<br/>DAOs · Auth · Export · Background Tasks"]
        MEDIA["mediasoup 0.20 SFU<br/>WorkerPool · RoomManager<br/>Router / Transport / Producer / Consumer"]
    end

    subgraph External["External Services"]
        CLOUD["Cloud Storage<br/>Google Drive · OneDrive · Dropbox"]
        AI["Claude API<br/>Document Recognition"]
        OAUTH["OAuth Providers<br/>Google · Facebook · GitHub<br/>LinkedIn · Microsoft"]
    end

    DB[("MongoDB 7<br/>18 collections")]

    UI -->|"REST / HTTP"| REST
    UI -->|"WebSocket"| WS
    UI -->|"WebRTC"| MEDIA
    AUTH --> REST
    AUTH --> WS
    REST --> SVC
    WS --> SVC
    WS --> MEDIA
    SVC --> DB
    SVC --> CLOUD
    SVC --> AI
    AUTH --> OAUTH

    style UI fill:#1565C0,color:#fff
    style REST fill:#0D47A1,color:#fff
    style WS fill:#0D47A1,color:#fff
    style AUTH fill:#4A148C,color:#fff
    style SVC fill:#00695C,color:#fff
    style MEDIA fill:#E65100,color:#fff
    style DB fill:#2E7D32,color:#fff
    style CLOUD fill:#37474F,color:#fff
    style AI fill:#37474F,color:#fff
    style OAUTH fill:#37474F,color:#fff
```

## Quick Start

```bash
# 1. Start infrastructure
docker-compose up -d    # MongoDB, Redis, MinIO, Coturn

# 2. Configure
cp .env.example .env    # Edit with your settings

# 3. Run backend
cargo build && cargo run   # API at http://localhost:3000

# 4. Run frontend
cd ui && bun install && bun run dev   # UI at http://localhost:5173
```

## Documentation

| Document | Description |
|----------|-------------|
| [Architecture](docs/architecture.md) | System design, crate graph, request flow |
| [Data Model](docs/data-model.md) | All 18 entities, ER diagram, indexes |
| [API Reference](docs/api.md) | REST endpoints, request/response schemas |
| [Features](docs/FEATURES.md) | Detailed feature descriptions (theming, mentions, roles, profiles, etc.) |
| [Frontend](docs/ui.md) | Routes, components, Pinia stores |
| [Real-Time](docs/real-time.md) | WebSocket protocol, presence, media signaling |
| [Testing](docs/testing.md) | Integration tests, Vitest unit tests, Playwright E2E specs |
| [Deployment](docs/deployment.md) | Docker Compose, environment variables |

## License

MIT
