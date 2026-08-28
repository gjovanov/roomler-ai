# Architecture

Roomler is one Rust server, one Vue SPA, and a small fleet of native binaries that
turn every enrolled machine into a remote-desktop target, a tunnel exit, and an
overlay-mesh node. This page is the map; every subsystem has a deeper doc linked
along the way. (Counts stamped *as of 0.3.0-rc.381.*)

## System at a glance

```mermaid
flowchart TB
    subgraph clients["Clients"]
        SPA["Vue 3 SPA<br/>chat · calls · remote viewer<br/>admin · observability"]
        CLI["roomler CLI<br/>forward · socks5 · exec · ping"]
        DESK["roomler-desktop<br/>tray companion (Tauri)"]
    end

    subgraph server["roomler.ai — Axum API pod(s)"]
        NG["nginx<br/>SPA + /api + /ws + /derp proxy"]
        API["roomler-ai-api<br/>REST + WebSocket + signalling"]
        SFU["mediasoup SFU<br/>(in-process workers)"]
        DERPD["/derp relay<br/>(pubkey-addressed WSS)"]
    end

    subgraph data["Data & infra"]
        MONGO[("MongoDB")]
        REDIS[("Redis<br/>pub/sub fan-out")]
        S3[("MinIO / S3<br/>files · recordings")]
        TURN["coturn<br/>TURN/STUN"]
        POP["derp-relay PoPs<br/>(standalone, DB-free)"]
    end

    subgraph fleet["Enrolled machines"]
        D1["roomlerd daemon<br/>capture · encode · input<br/>tunnel exit · overlay node"]
        D2["roomlerd daemon"]
    end

    SPA -->|REST /api| NG --> API
    SPA <-->|"/ws (user role)"| API
    SPA <-->|"WebRTC RTP"| SFU
    CLI <-->|"/ws (tunnel-client role)"| API
    DESK <-->|LocalAPI| D1
    D1 <-->|"/ws?role=agent (outbound WSS)"| API
    D2 <--> API
    API --- MONGO & REDIS & S3
    SPA <==>|"WebRTC P2P — remote desktop"| D1
    CLI <==>|"QUIC / WebRTC DC — tunnels"| D2
    D1 <-.->|"WireGuard overlay"| D2
    SPA & D1 & D2 -.->|fallback| TURN
    D1 & D2 -.->|"both UDP-blocked"| DERPD
    D1 & D2 -.-> POP
```

## Control plane vs three data planes

The server is a **coordination point, not a data path**. Each pillar has its own
data plane with its own transport and encryption; the server only ever brokers
sessions, enforces policy, and (for conferencing) mixes nothing — the SFU forwards
RTP without decoding it.

```mermaid
flowchart LR
    subgraph cp["Control plane — always through roomler.ai"]
        C1["auth · enrollment · consent<br/>ACL policy · signalling · audit"]
    end

    subgraph dp["Data planes"]
        P1["💬 Conference media<br/>browser ⇄ mediasoup SFU<br/>WebRTC RTP (DTLS-SRTP)"]
        P2["🖥️ Remote desktop<br/>browser ⇄ roomlerd, P2P<br/>WebRTC RTP + DataChannels"]
        P3["🌐 Overlay & tunnels<br/>node ⇄ node WireGuard<br/>tunnels over QUIC / WebRTC DC"]
    end

    C1 -.->|"session setup only"| P1 & P2 & P3
```

| Plane | Endpoints | Direct path | Fallback ladder | Server sees |
|---|---|---|---|---|
| Conference | browser ⇄ server SFU | always via SFU | — | encrypted RTP it routes (SFU model) |
| Remote desktop | browser ⇄ `roomlerd` | ICE P2P | host → STUN srflx → TURN (UDP → TCP/TLS :443) | signalling only; TURN relays ciphertext |
| Overlay mesh | `roomlerd`/CLI ⇄ peer | LAN → public → srflx hole-punch | TURN relay (QUIC-upgraded) → DERP over WSS :443 | netmaps + relay grants; WG ciphertext only |

Key invariants:

- **The server never sees pixels, keystrokes, clipboard, files, or overlay
  plaintext.** Remote-desktop media and input ride P2P WebRTC (E2E-encrypted);
  overlay packets are WireGuard-encrypted end to end; TURN and DERP forward bytes
  they cannot decrypt.
- **Everything is tenant-scoped.** Every route nests under
  `/api/tenant/{tenant_id}/…`, every collection carries `tenant_id`, and the six
  JWT audiences (below) cannot impersonate one another.
- **Agents dial out only.** A `roomlerd` daemon holds one outbound WSS control
  connection; no inbound port is ever required on an enrolled machine.

## Workspace map

One Cargo workspace builds the server and every native binary.
`config ← db ← remote_control ← services ← api` remains the server spine; the
agent stack layers on shared leaf crates so thin clients never link the world.

```mermaid
flowchart BT
    config["crates/config<br/>settings (ROOMLER__ env)"]
    db["crates/db<br/>Mongo models + indexes"]
    rc["crates/remote_control<br/>signalling · consent · Hub¹ · ACL shapes"]
    services["crates/services<br/>DAOs · auth · media (mediasoup) · billing"]
    api["crates/api<br/>Axum: REST + /ws + /derp"]
    tests["crates/tests<br/>integration suite"]

    localapi["crates/localapi<br/>LocalAPI wire types + client"]
    agentcore["crates/agent-core<br/>config · enrollment · machine-id"]
    tunnelcore["crates/tunnel-core<br/>tunnels · transports · overlay (WG)"]
    tcpturn["crates/tcp-turn-conn<br/>TURNS/TCP adapter"]
    derprelay["crates/derp-relay<br/>standalone DERP PoP"]
    setupcore["crates/roomler-setup-core<br/>installer mechanics"]

    roomlerd["agents/roomlerd → roomlerd<br/>the daemon"]
    tunnel["agents/roomler-cli → roomler<br/>tunnel CLI (surface lives in its lib)"]
    shim["agents/roomler-cli-shim<br/>roomler.exe on daemon hosts → re-execs roomlerd cli"]
    tray["agents/roomler-desktop → roomler-desktop"]
    setup["agents/roomler-setup<br/>install wizard (Tauri)"]

    db --> config
    rc --> db
    services --> rc
    api --> services
    tests --> api & roomlerd

    tunnelcore --> localapi & tcpturn & rc
    agentcore --> config
    roomlerd --> tunnelcore & agentcore
    tunnel --> tunnelcore
    tray --> localapi & agentcore
    setup --> setupcore
    derprelay --> rc
```

¹ `remote_control`'s Mongo-backed parts (audit DAO, session Hub) sit behind a
default-on `server` cargo feature; agent-side consumers disable it, which keeps the
MongoDB driver out of every shipped native binary.

**Vendored forks** (workspace-excluded, wired via `[patch.crates-io]` — the *why*
is documented at the top of the root `Cargo.toml`):

| Fork | Reason |
|---|---|
| `crates/vendored/rtp` | H.265 payloader fix — upstream drops NALs after VPS/SPS/PPS aggregation, PLI-storming Chrome |
| `crates/vendored/webrtc-ice` | TURNS-over-TLS-over-TCP relay candidates (upstream closed NOT_PLANNED) — corp networks that block all UDP |
| `crates/vendored/webrtc` | exposes SCTP `a_rwnd` so native⇄native DataChannels advertise 8 MiB and stay link-bound at high BDP |
| `crates/vendored/wintun-bindings` | keeps the Windows NetworkList registry entry on drop |

## The native stack on an enrolled machine

| Binary | Role |
|---|---|
| **`roomlerd`** | The daemon: WS signalling, per-session WebRTC peers, capture → encode → RTP/DC, input injection, clipboard/file/apps/audio channels, tunnel *target* and tunnel *client* sides, overlay node runtime, LocalAPI server, loopback TURN host, watchdog, self-updater, Windows SCM/SystemContext machinery |
| **`roomler`** | The CLI. On tunnel-only hosts it is the full standalone binary; on daemon hosts the MSI/.deb installs a ~150 KB shim that re-execs `roomlerd cli` — one command surface (`roomler_tunnel::cli`), never version-skewed against the daemon |
| **`roomler-desktop`** | Tauri tray companion: node status, peers, tunnels, consent prompts — a thin LocalAPI client with zero transport crates |
| **`roomler-setup`** | The unified install wizard ([installation.md](installation.md)) |

The **LocalAPI** (named pipe `\\.\pipe\roomler` on Windows, `$XDG_RUNTIME_DIR/roomler.sock`
elsewhere) is the on-host control surface all three clients share — status, peers,
flows, routes, consent decisions. Protocol reference: [tunnels.md](tunnels.md).

## Request flow & auth

```
Browser/CLI/daemon ──► nginx ──► Axum router
                                   ├─ per-IP governor (all /api)
                                   ├─ per-(IP, account) brute-force gate (login/register)
                                   ├─ JWT middleware (audience-checked)
                                   └─ handler ─► services (DAO) ─► MongoDB
                                              └─► Redis pub/sub ─► every pod ─► /ws clients
```

Six JWT audiences, all signed with the server secret, none interchangeable:
`Access` / `Refresh` (users), `Enrollment` (single-use, 10 min, mints an agent),
`Agent` (the daemon's long-lived credential), `TunnelEnrollment` / `TunnelClient`
(the CLI's pair). Verifiers reject cross-audience tokens; see
[api.md](api.md#authentication) for the token flows.

Real-time delivery: WebSocket sessions are pod-local; chat, presence, and
notifications fan out across pods via Redis pub/sub. Long-lived agent/tunnel/DERP
sockets are kept tenant-affine by the front load balancer (consistent hash on the
tenant id) so a tenant's controllers, agents, and mediasoup rooms co-locate on one
pod — the multi-pod design is settled in
[multi-pod-scale-out.md](multi-pod-scale-out.md).

## Frontend

Vue 3 + Vuetify 3 SPA (Vite, TypeScript, Pinia setup stores). The remote-desktop
viewer is the deepest component: WebCodecs decode workers, five render paths,
clipboard/file bridges — mapped in [ui.md](ui.md). Observability (mesh graph,
usage, relay stats) renders with d3. i18n is wired but ships English only today.

## Deployment shape

- **One Docker image** — `rust:1.88-bookworm` builds the API, `oven/bun` builds the
  SPA, `debian:trixie-slim` runs nginx + the binary ([deployment.md](deployment.md)).
- **Dev stack** — `docker compose up -d`: MongoDB (:27019), Redis, MinIO, coturn.
- **Native artifacts** — MSI / `.deb` / `.pkg` / tarballs built by tag-triggered
  GitHub workflows and served through the server's own installer proxies
  ([installation.md](installation.md)).
- **Relay infrastructure** — coturn for TURN; `crates/derp-relay` builds the
  standalone, DB-free regional DERP PoPs (Ed25519 ticket auth).

## Where to go deeper

| Topic | Doc |
|---|---|
| Remote-desktop design (protocol, consent, latency budget) | [remote-control.md](remote-control.md) |
| Encoder cascade & rate control | [encoders.md](encoders.md) |
| Overlay carriers, NAT traversal, DERP | [overlay-communication.md](overlay-communication.md), [overlay-nat-traversal.md](overlay-nat-traversal.md) |
| Tunnels, LocalAPI, CLI | [tunnels.md](tunnels.md) |
| Multi-org devices | [multi-org.md](multi-org.md) |
| HTTP + WS surface | [api.md](api.md), [real-time.md](real-time.md) |
| Data model | [data-model.md](data-model.md) |
