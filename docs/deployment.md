# Deployment

Deploying the Roomler server and its supporting infrastructure. The native fleet
(agents, CLI, wizard) is *not* part of the server image — it ships through GitHub
Releases and the server's installer proxies ([installation.md](installation.md)).
*As of 0.3.0-rc.381.*

## Topology

```mermaid
flowchart TB
    LB["front reverse proxy / LB<br/>TLS · consistent-hash on tenant id"]
    subgraph pod["API pod (1..N replicas)"]
        NG["nginx — SPA files ·<br/>/api /ws /derp proxy · security headers"]
        BIN["roomler-ai-api (Rust)<br/>REST · WS · mediasoup workers"]
    end
    MONGO[("MongoDB")]
    REDIS[("Redis — pub/sub fan-out<br/>+ online registry")]
    MINIO[("MinIO / S3")]
    COTURN["coturn (TURN/STUN)"]
    DERP["derp-relay PoPs<br/>(standalone, DB-free, per region)"]

    LB --> NG --> BIN
    BIN --- MONGO & REDIS & MINIO
    BIN -.->|"mints ephemeral creds"| COTURN
    BIN -.->|"Ed25519 tickets"| DERP
```

## The server image

One multi-stage `Dockerfile`:

1. `rust:1.88-bookworm` — builds `roomler-ai-api` (+ `derp-relay`). Two build
   args select the composition (FR-69 P8): `PROFILE` (`full` | `collab` | `remote` |
   `mesh` | `access`, the Cargo feature aggregate the api crate is built with) and
   `SAAS` (`1` adds the hosted service's billing + newsletter module). **The hosted
   build passes neither** — the defaults are `full` + `SAAS=1`, so the deploy recipe
   below is unchanged; the self-host publish workflow passes `SAAS=0` and asserts it.
2. `oven/bun:1` — builds the Vue SPA
3. `debian:trixie-slim` — runtime: **nginx + the binary in one image**, SPA at
   `/var/www/roomler-ai`, nginx config from `files/nginx-pod.conf` (SPA fallback,
   API/WS proxy, security headers incl. HSTS + CSP), `EXPOSE 80`

## Development stack

```bash
docker compose up -d
```

| Service | Port | Purpose |
|---|---|---|
| `mongo:7` | 27019→27017 | database (dev credentials in the compose file) |
| `redis:7-alpine` | 6379 | pub/sub + presence |
| `minio/minio` | 9000 (API) / 9001 (console) | S3-compatible file storage |
| `coturn/coturn` | host network | TURN relay (`turnserver.conf` — rotate the shared secret!) |

Then `cargo run --bin roomler-ai-api` (API :3000) and `cd ui && bun run dev`
(SPA :5000, proxying `/api` + `/ws` to :5001).

## Configuration

Everything is env-configurable with the `ROOMLER__` prefix (double underscore =
nesting), loaded via the `config` crate. The ones that matter first:

| Variable | Purpose |
|---|---|
| `ROOMLER__DATABASE__URL` | MongoDB connection string |
| `ROOMLER__JWT__SECRET` | **Must be set in production** — with `ROOMLER__APP__ENVIRONMENT=production` the server refuses to boot on the default |
| `ROOMLER__JWT__PREVIOUS_SECRETS` | Comma-separated retired secrets that still **verify** but no longer sign. See [Rotating the JWT secret](#rotating-the-jwt-secret) |
| `ROOMLER__APP__FRONTEND_URL` | Public origin (also the CORS default — unset `cors_origins` allows only this origin) |
| `ROOMLER__APP__CORS_ORIGINS` | Explicit allow-list; `"*"` = deliberate permissive mode (warns) |
| `ROOMLER__TURN__SHARED_SECRET` | coturn REST-auth secret (never committed) |
| `ROOMLER__MEDIASOUP__ANNOUNCED_IP_MAP` | `<node_ip>=<public_ip>,…` — per-pod announced IP resolution for multi-node clusters |
| `ROOMLER__STRIPE__*` / `ROOMLER__CLAUDE__*` / `ROOMLER__S3__*` / SMTP / OAuth | Integrations |

Rate limiting (per-IP governor + per-account brute-force gate) and JWT TTLs are
also settings — see `crates/config/src/settings.rs` for the full surface.

### Rotating the JWT secret

One secret signs six audiences (access, refresh, agent-enrollment, agent,
tunnel-enrollment, tunnel-client). Changing it used to invalidate every live
token at once — including every enrolled agent's **one-year** token, i.e. a
fleet-wide re-enrollment by hand. `previous_secrets` makes it a rolling change:

```bash
# 1. Both verify; only the new one signs. Restart/roll the pods.
ROOMLER__JWT__SECRET=<new>
ROOMLER__JWT__PREVIOUS_SECRETS=<old>

# 2. Wait out the longest TTL still in flight, or re-issue ahead of it:
#    access 7 d · refresh 30 d · agent + tunnel-client 1 YEAR.
#    Agent tokens are re-minted on re-enrollment; there is no bulk re-issue yet,
#    so in practice step 3 waits a year unless you re-enroll.

# 3. Drop the old key. Only now is the old secret actually powerless.
ROOMLER__JWT__PREVIOUS_SECRETS=
```

Startup logs `jwt: signing key signing_kid=… verify_keys=N`. A correct rotation
reads as **`verify_keys` 1 → 2 with a changed `signing_kid`**; a changed
`signing_kid` with `verify_keys=1` is the flag day — every live token just died.

⚠️ **This is not revocation.** Until step 3, tokens signed with the old secret
are still accepted, so a *leaked* secret is not contained by step 1 alone. What
rotation buys is that step 3 is reachable at all: an emergency cut-over can be
staged (re-issue on the new key, then drop the old) instead of being one
outage-shaped event.

⚠️ Listing the default `change-me-in-production` in `previous_secrets` is
refused under `ROOMLER__APP__ENVIRONMENT=production` — a retired secret forges
exactly as well as a current one.

⚠️ Tokens minted before `kid` shipped carry no key hint, so they are tried
against every configured key. That is what lets a year-old agent token survive
a rotation, and it is why the fallback is not an optimisation to remove.

## Health & probes

| Endpoint | Meaning |
|---|---|
| `GET /health` | Liveness/startup — cheap process-alive 200 (never flaps on dependency blips) |
| `GET /health/ready` | Readiness — Mongo ping + Redis round-trip + a live pub/sub subscription; 503 with per-check detail otherwise |

## Scaling beyond one pod

The multi-pod design is settled and documented in
[multi-pod-scale-out.md](multi-pod-scale-out.md). The short version:

- WS sessions, the rc/tunnel hubs, DERP sockets, and mediasoup rooms are
  **pod-local**; chat/notifications/presence fan out via Redis.
- The front LB keeps a tenant's users, agents, and rooms on one pod with a
  **consistent hash on the tenant id** (`/ws` and `/derp` accept a `tid=` hint);
  plain HTTP keeps per-request failover.
- Startup maintenance is leader-gated behind a Mongo lease; the online registry
  (Redis) backs offline push/email dedupe.

## Relay infrastructure

- **coturn** — TURN/STUN for remote-desktop and tunnel fallback paths. The server
  mints ephemeral HMAC credentials (`/api/turn/credentials`); multi-region
  topology is served from `/api/relay/regions`.
- **DERP PoPs** — `cargo build -p derp-relay` produces the standalone regional
  relay: DB-free, no JWT secret, authenticates agents by server-minted Ed25519
  tickets. One small VM per region is enough; it forwards WireGuard ciphertext it
  cannot read.

## Release pipelines (native fleet)

Tag-triggered GitHub workflows build, sign, and publish the native artifacts;
the server proxies the downloads and gets a cache-bust ping
(`POST /api/releases/refresh`) on publish:

| Workflow | Tag | Artifacts |
|---|---|---|
| `release-agent.yml` | `agent-v*` | Windows MSIs (perUser + perMachine) + `roomler-desktop` companion; Linux `.deb`/tarball (x86_64 **and** aarch64); macOS `.pkg` (arm64) |
| `release-tunnel.yml` | `tunnel-v*` | `roomler` CLI: Windows zip, Linux tarball + `.deb`, macOS universal tarball |
| `release-setup.yml` | `setup-v*` | The install wizard: Linux/macOS tarballs, signed Windows EXE zip |

All assets carry `.sha256`, GPG `.asc`, and SLSA provenance; releases are
published non-prerelease so `/releases/latest` stays resolvable for the fleet's
auto-updaters.
