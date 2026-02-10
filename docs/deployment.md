# Deployment

## Docker Compose Services

The `docker-compose.yml` provides all infrastructure dependencies:

| Service | Image | Ports | Purpose |
|---------|-------|-------|---------|
| Mongo | `mongo:7` | 27019 | Primary database |
| Redis | `redis:7-alpine` | 6379 | Cache and pub/sub |
| MinIO | `minio/minio:latest` | 9000 (API), 9001 (Console) | S3-compatible object storage |
| Coturn | `coturn/coturn:latest` | host network | TURN server for NAT traversal |

### Starting Infrastructure

```bash
docker-compose up -d
```

### Default Credentials

| Service | Username | Password |
|---------|----------|----------|
| Mongo | `roomler` | `roomler_pass` |
| MinIO | `minioadmin` | `minioadmin` |

## Environment Variables

All configuration is via environment variables prefixed with `ROOMLER__` using `__` as the separator.

### Application

| Variable | Default | Description |
|----------|---------|-------------|
| `ROOMLER__APP__HOST` | `0.0.0.0` | Bind address |
| `ROOMLER__APP__PORT` | `3000` | HTTP port |

### Database

| Variable | Default | Description |
|----------|---------|-------------|
| `ROOMLER__DATABASE__URL` | `mongodb://localhost:27019` | MongoDB connection string |
| `ROOMLER__DATABASE__NAME` | `roomler2` | Database name |

### JWT

| Variable | Default | Description |
|----------|---------|-------------|
| `ROOMLER__JWT__SECRET` | `change-me-in-production` | JWT signing secret |
| `ROOMLER__JWT__ACCESS_TOKEN_TTL_SECS` | `3600` | Access token TTL (1 hour) |
| `ROOMLER__JWT__REFRESH_TOKEN_TTL_SECS` | `604800` | Refresh token TTL (7 days) |
| `ROOMLER__JWT__ISSUER` | `roomler2` | JWT issuer claim |

### Redis

| Variable | Default | Description |
|----------|---------|-------------|
| `ROOMLER__REDIS__URL` | `redis://127.0.0.1:6379` | Redis connection URL |

### S3 / MinIO

| Variable | Default | Description |
|----------|---------|-------------|
| `ROOMLER__S3__ENDPOINT` | `http://localhost:9000` | S3 endpoint |
| `ROOMLER__S3__ACCESS_KEY` | `minioadmin` | Access key |
| `ROOMLER__S3__SECRET_KEY` | `minioadmin` | Secret key |
| `ROOMLER__S3__BUCKET` | `roomler2` | Bucket name |
| `ROOMLER__S3__REGION` | `us-east-1` | Region |

### mediasoup (Phase 5)

| Variable | Default | Description |
|----------|---------|-------------|
| `ROOMLER__MEDIASOUP__NUM_WORKERS` | `2` | Worker process count |
| `ROOMLER__MEDIASOUP__LISTEN_IP` | `0.0.0.0` | Bind address |
| `ROOMLER__MEDIASOUP__ANNOUNCED_IP` | `127.0.0.1` | Public IP for ICE |
| `ROOMLER__MEDIASOUP__RTC_MIN_PORT` | `40000` | RTC UDP port range start |
| `ROOMLER__MEDIASOUP__RTC_MAX_PORT` | `49999` | RTC UDP port range end |

### TURN Server

| Variable | Default | Description |
|----------|---------|-------------|
| `ROOMLER__TURN__URL` | _(none)_ | TURN server URL |
| `ROOMLER__TURN__USERNAME` | _(none)_ | TURN username |
| `ROOMLER__TURN__PASSWORD` | _(none)_ | TURN password |

### Claude API (AI)

| Variable | Default | Description |
|----------|---------|-------------|
| `ROOMLER__CLAUDE__API_KEY` | _(none)_ | Claude API key for document recognition |
| `ROOMLER__CLAUDE__MODEL` | `claude-sonnet-4-5-20250929` | Model ID |
| `ROOMLER__CLAUDE__MAX_TOKENS` | `4096` | Max response tokens |

## Configuration Loading

Settings are loaded in priority order (later sources override earlier):

1. `config/default.toml` (optional)
2. `config/local.toml` (optional, gitignored)
3. Environment variables (`ROOMLER__` prefix, `__` separator)
4. Hardcoded defaults in `Settings::load()`

The `config` crate handles merging. The separator `__` maps to nested config keys:
- `ROOMLER__JWT__SECRET` → `jwt.secret`
- `ROOMLER__DATABASE__URL` → `database.url`

## Production Build

### Backend

```bash
cargo build --release
# Binary at target/release/roomler2
```

### Frontend

```bash
cd ui
npm run build
# Output in ui/dist/
```

The backend can serve the built frontend by setting `ROOMLER__APP__STATIC_DIR=ui/dist`.

## Tenant Plans

| Plan | Max Members (default) | File Upload Limit (default) |
|------|----------------------|---------------------------|
| Free | 100 | 10 MB |
| Pro | 100 | 10 MB |
| Business | 100 | 10 MB |
| Enterprise | 100 | 10 MB |

Limits are configurable per-tenant via `TenantSettings`. Plan-based differentiation is intended to be configured by the operator.

## Health Check

```bash
curl http://localhost:3000/health
# {"status":"ok","version":"0.1.0"}
```

## Future Infrastructure

- **mediasoup workers** -- SFU for WebRTC audio/video (Phase 5, dependency currently commented out)
- **Horizontal scaling** -- Redis pub/sub for cross-instance WebSocket broadcasting
