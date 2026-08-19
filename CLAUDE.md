# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**Roomler AI is three products on one platform** — one Rust workspace, one Vue SPA, one native daemon per enrolled machine. `docs/README.md` is the navigable doc index and mirrors this split.

| # | Pillar | What it is | Deep docs |
|---|---|---|---|
| 1 | **Remote desktop** — TeamViewer-class, in Rust | A browser controls an enrolled machine: capture → HW encode → WebRTC P2P (RTP *or* reliable DataChannel + WebCodecs) + input injection, clipboard, file transfer, remote apps, audio, pre-logon/SystemContext control | `docs/remote-control.md`, `docs/encoders.md` |
| 2 | **Networking** — overlay mesh (Tailscale-class) **+** userspace tunnels (ngrok-class) | A private WireGuard L3 mesh between all enrolled machines (`100.64.0.0/10`, MagicDNS, subnet routers, exit nodes, ACLs) **and** on-demand `host:port` forwards / SOCKS5 (TCP + UDP) over QUIC or WebRTC DataChannels, both driven by a policy-gated coordination server | `docs/overlay-communication.md` (start here), `docs/tunnels.md`, `docs/overlay-nat-traversal.md`, `docs/multi-org.md` |
| 3 | **Collaboration** — chat · video conferencing · teamwork | Rooms/channel tree, threaded chat with reactions/mentions/emoji, mediasoup SFU calls with recording + in-call chat, files, invites, roles, notifications, exports, billing | `docs/real-time.md`, `docs/ui.md`, `docs/use-cases.md` |

Stack: Rust (Axum) + MongoDB + Redis + MinIO/S3 + Vue 3/Vuetify 3 + Pinia + mediasoup (WebRTC SFU) + webrtc-rs (P2P remote control) + boringtun (WireGuard overlay) + quinn (QUIC tunnels) + coturn/DERP (relays).

Everything on an enrolled machine ships in **ONE** native daemon — `roomlerd`, built from `agents/roomler-agent`. Remote-desktop target, tunnel exit, tunnel client and overlay node are the *same process*; `roomler` (CLI), `roomler-desktop` (tray) and `roomler-setup` (wizard) are its satellites. See "The native stack" in `docs/architecture.md`.

**Core invariant across all three pillars: the server coordinates, it never carries plaintext.** Remote-desktop pixels/keystrokes and overlay packets are end-to-end encrypted (WebRTC DTLS-SRTP / WireGuard Noise-IK); TURN and DERP forward bytes they cannot decrypt; only conference media traverses the server, and the SFU forwards RTP without decoding it. Agents dial **out** only — no inbound port is ever required on an enrolled machine.

The generic patterns (auth, routing, DB, testing, deployment) come first below; the **deep per-pillar sections are near the bottom of this file** — "Pillar 1 — Remote Desktop", "Pillar 2 — Networking", "Pillar 3 — Collaboration", then "Node stack". Counts and version-stamped notes reflect `0.3.0-rc.417`.

### Pillar 2 design goal — read this before touching networking code

> **The Roomler networking solution is a resilient, secure private network that "just works" — at maximum performance and lowest achievable latency — across varied and complex real-world network infrastructures and setups.**

"Just works" is the acceptance bar, not a slogan. Concretely it must hold on all of:

- **Private vs corporate machines** — an unmanaged laptop and a Group-Policy-locked corporate desktop (locked firewall, TLS-inspecting middleboxes, EDR/ESET, no admin rights on the network stack) must both land in the same mesh.
- **Standalone VPN vs corporate VPN** — a consumer VPN client (Surfshark/WireGuard/OpenVPN) and an enterprise full-tunnel client (AnyConnect and friends) both reroute *and reap* routes underneath us; connect/disconnect transitions must not wedge the node, and must never wedge the **host** (SSH/RDP into that box has to survive).
- **Servers and clusters** — headless Linux servers, k8s and OpenShift nodes, containers, WSL: no desktop session, no interactive user, hostile CNI/iptables interaction.
- **Cross-platform** — Windows, Linux, macOS are first-class; per-OS mechanics differ (Wintun + WFP, netlink + rt tables, utun + LaunchAgent), the *behaviour* must not.

The engineering commitments that follow from it — all already load-bearing in the code, don't regress them:

1. **Best carrier that works, always measured, never assumed.** The cascade is LAN → direct-public → srflx hole-punch → single-relay (TURN, QUIC-upgraded) → DERP over WSS :443. Selection is a **server verdict from measured CapVectors** on an always-on DERP floor; heuristics may *detect*, they never *decide*. Never ratchet: a node that fell to relay must keep re-attempting direct (MBB — make-before-break, then relentless re-upgrade).
2. **A floor that always connects.** If every UDP path is blocked, DERP over TLS :443 still carries the mesh. Connectivity is never all-or-nothing.
3. **Never self-wedge.** Route/exit/firewall changes install carrier + control-plane exemptions FIRST and *withhold* the change if they can't — a mesh feature must never cost the operator their own remote access to the box. Route guards re-assert; boot reconcilers heal stale state after a hard exit.
4. **No OS privileges required as a fallback.** When an OS TUN or routing table isn't available or is owned by someone else (corp full-tunnel VPN, container, locked-down host), the **userspace netstack** mode gives the same mesh through a loopback SOCKS5 front with zero routing changes.
5. **Default-deny, tenant-scoped, end-to-end encrypted.** Overlay ACLs, tunnel ACLs, and an agent-local ACL that survives a compromised server. Every decision audited.
6. **Field-validated, not CI-validated.** Networking changes are proven on the real fleet across the real topologies above — corp-VPN laptop, GPO-firewalled desktop, headless Linux server, WSL, dev box — via `roomler exec` after every roll. CI green ≠ done.

## Commands

```bash
# Development
cargo run --bin roomler-ai-api         # Start backend (port 3000)
cd ui && bun run dev                   # Vite dev server (port 5000, proxies to 5001)
cd ui && bun run build                 # Production UI build (includes vue-tsc --noEmit)

# The node daemon `roomlerd` (agents/roomler-agent) — remote-desktop target + tunnel exit + overlay node
cargo build -p roomler-agent --release --features full                    # capture + encode + input + clipboard + audio (SW encoder)
cargo build -p roomler-agent --release --features full-hw                 # + MF HW encoder, WGC capture, viewer indicator (Windows production)
cargo build -p roomler-agent --release --features full-hw,ffmpeg-encoder  # + NVENC/QSV/AMF via vendored minimal FFmpeg (HEVC/AV1/vp9_qsv)
cargo build -p roomler-agent --release --features overlay-l3              # WireGuard mesh w/ OS TUN + routes
cargo build -p roomler-agent --release --features overlay-netstack        # userspace netstack + loopback SOCKS5 (no OS routing)
cargo build -p roomler-agent --release                                    # signalling-only (CI / integration tests)
./target/release/roomlerd enroll --server <url> --token <enrollment-jwt> --name <label>
./target/release/roomlerd run
./target/release/roomlerd run --encoder software|hardware  # openh264 vs the MF/FFmpeg HW cascade
./target/release/roomlerd encoder-smoke --encoder hardware [--codec hevc]   # offline: 10 synthetic frames, prints the cascade's decisions
./target/release/roomlerd cli <args>   # the `roomler` command surface, hosted in the daemon (what the shim re-execs)
./scripts/dev-xvfb.sh                  # capture smoke test via a virtual framebuffer

# The `roomler` CLI (agents/roomler-tunnel — standalone on tunnel-only hosts, a ~150 KB shim on daemon hosts)
roomler enroll --server <url> --token <tunnel-enrollment-jwt>
roomler forward --agent <name> --local 127.0.0.1:5432 --remote db:5432   # ngrok-style TCP forward
roomler socks5 --local 127.0.0.1:1080 [--agent <name>]                   # per-agent or tenant-wide mesh SOCKS5 (TCP + UDP)
roomler route add|rm|ls|enable|disable                                   # declared routes the daemon supervises
roomler status | peers | flows | netcheck | ping <peer> | logs           # node + mesh diagnostics
roomler exec <host> -- <cmd>                                             # fleet RPC over the control WS (4 default-deny gates)
roomler diag host|pair                                                   # diagnostic bundles

# Testing
cargo test -p roomler-ai-tests         # All integration tests (33 modules, requires MongoDB+Redis)
cd ui && bun run test:unit             # Vitest unit tests (30 spec files)
cd ui && bun run test:unit:coverage    # Vitest with coverage
cd ui && bun run e2e                   # Playwright E2E tests (32 spec files)

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

`--features full` (or the individual `scrap-capture` / `openh264-encoder` / `enigo-input` / `audio` flags) pulls in system deps:

```bash
# Linux (scrap-capture + audio)
sudo apt install -y libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev libasound2-dev

# OpenH264 is compiled from C source on first build — slow but no runtime lib needed.
# ffmpeg-encoder needs a vendored FFmpeg tree on PKG_CONFIG_PATH (+ vcvars on Windows for bindgen).
```

Default build (no features) compiles on any rust:bookworm image and produces a signalling-only daemon useful for CI / integration tests, but not usable in production (no capture, no input, no mesh).

**Feature map** (the ones that change behaviour, not just size):

| Feature | Pillar | Effect |
|---|---|---|
| `media` / `full` / `full-hw` | 1 | capture + encode + input + clipboard + audio; `full-hw` adds MF HW encoder, WGC capture, viewer indicator (Windows production MSI) |
| `ffmpeg-encoder` | 1 | NVENC / QSV / AMF backends via `ffmpeg-next` against a **minimal vendored FFmpeg** (exactly 10 encoders; names locked by unit tests) |
| `vp9-444` | 1 | libvpx VP9 4:4:4 for the DataChannel-bypass video transport |
| `system-context` | 1 | Windows pre-logon / secure-desktop capture + input (gated by `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP`) |
| `overlay-l3` | 2 | WireGuard mesh with a real OS TUN + routing table changes (the privileged/default path) |
| `overlay-netstack` | 2 | userspace netstack + loopback SOCKS5 front — the mesh with **zero OS routing**, for corp full-tunnel VPN hosts and containers. Independent of `overlay-l3`; a build may carry either or both, config picks at runtime |
| `ssh-server` | 2 | Roomler SSH (russh, implies `overlay-netstack`). **Opt-in per build and NOT in the release feature sets** — +1.86 MiB / ~99 crates / a second RustCrypto generation. russh must stay `default-features = false, features = ["ring"]`: defaults pull aws-lc-rs and break tunnel-core's ring-only invariant |

⚠️ `cargo test -p roomler-agent --lib` **silently skips** the overlay tests — add `--features overlay-l3`.

### Encoder selection

Preference resolution: **CLI `--encoder` > env (`ROOMLER_NODE_ENCODER` / `ROOMLER_AGENT_ENCODER`) > `encoder_preference` in config.toml > `auto`**. Values: `auto` | `hardware` (`hw`/`mf`) | `software` (`sw`/`openh264`). On Windows `auto` runs the DXGI-adapter × MFT probe-and-rollback cascade (activate + encode ONE probe frame, roll back on failure) before falling back to openh264; with `ffmpeg-encoder` the vendor backends join the cascade. Escape hatch `ROOMLER_AGENT_HW_AUTO=0` reverts to openh264-first without a rebuild. Current reference is **`docs/encoders.md`** (codec × platform × backend matrix, rate control, capture backends, viewer decode paths); `docs/remote-control.md` §17-19 are historical appendices.

## Architecture

Pillar ownership is marked **[1]** remote desktop / **[2]** networking / **[3]** collaboration / **[·]** shared.

```
crates/
  config/           → [·] Settings (env vars via ROOMLER__ prefix, config crate)
  db/               → [·] MongoDB models + indexes (37 collections) + native driver v3.2
  services/         → [3] Business logic: auth, DAOs, media (mediasoup), export, background tasks, OAuth, push, email, Stripe, Giphy
  remote_control/   → [1] Remote-desktop subsystem: Hub, `rc:*` signalling, consent, audit, TURN creds. ALSO the shared wire home for [2]:
                          the `rc:overlay.*` / `rc:tunnel.*` / `rc:rpc.*` variants + the canonical ACL rule shapes (`dst_matches`/`host_matches`).
                          Its Mongo-backed parts (audit DAO, session Hub, `Error::Mongo`) sit behind a default-ON `server` feature that
                          agent-side consumers disable — that keeps the mongodb driver out of every shipped native binary.
  tunnel-core/      → [2] THE networking crate: tunnel forwards/SOCKS5/mux/policy/LocalAPI-driver + `overlay/` (WireGuard data plane,
                          carrier plane, disco, netmap, router, netstack, DNS/MagicDNS, WFP, NAT probing, warm relay, path monitor)
                          + `transport/` (quic, webrtc_dc, wireguard, derp, relay, stun, turn_host)
  tcp-turn-conn/    → [2] TURNS-over-TLS-over-TCP Conn adapter — the substance of the webrtc-ice fork, extracted so the vendored tree stays a mechanical patch
  derp-relay/       → [2] Standalone regional DERP PoP binary: ticket-authenticated, DB-free
  localapi/         → [2] LocalAPI protocol LEAF crate (wire types, client, dispatch) — re-exported as `tunnel_core::localapi`; thin clients dep it directly (P3e lever E)
  agent-core/       → [·] Daemon-free agent building blocks: config, enrollment, machine-id, logging, sentinels, forward ACL — re-exported by `roomler-agent` under the old `crate::` paths; the desktop companion deps THIS, never the full agent (P3e lever E)
  roomler-setup-core/ → [·] Installer mechanics, event-shape-free (lib `wizard_shared`)
  api/              → [·] Axum HTTP/WS server: ~37 route modules + /ws + /derp + /health
  tests/            → [·] Integration tests (33 modules; spawns real servers + drives the agent library in-process)
  vendored/         → [·] rtp (H.265 payloader fix) · webrtc-ice (TURNS/TCP) · webrtc (SCTP a_rwnd) · wintun-bindings — wired via [patch.crates-io]; the *why* is at the top of the root Cargo.toml
agents/
  roomler-agent/    → [1][2] `roomlerd`, THE daemon. Remote-desktop target (capture/encode/input/clipboard/files/apps/audio,
                          SystemContext) + tunnel exit AND client (`tunnel/`) + overlay node (`overlay.rs`, `derp.rs`) +
                          LocalAPI server + loopback TURN host + watchdog + self-updater + Windows SCM machinery
  roomler-tunnel/   → [2] Tunnel client. Bin `roomler` (tunnel-only hosts); the whole command surface lives in the LIB (`cli.rs`) so the daemon can host it too
  roomler-cli-shim/ → [2] Bin `roomler-shim`, installed BY THE MSI/.deb as `roomler` on daemon hosts: re-execs `roomlerd cli` (P3e lever D)
  roomler-agent-tray/ → [·] `roomler-desktop` — Tauri tray companion (status, peers, tunnels, consent). Thin LocalAPI client, ZERO transport crates
  roomler-setup/    → [·] The unified install wizard (Tauri 2, lib `wizard_app`)
ui/
  src/
    api/            → HTTP client (client.ts)
    components/     → 11 categories (admin, chat, common, conference, enroll, invite, landing, layout, remote, rooms, stats)
    composables/    → 13 hooks (useAuth, useWebSocket, useRemoteControl, useMarkdown, usePolling, usePageViews, …)
    stores/         → 20 Pinia stores (setup store pattern — incl. agents, tunnelClients, tunnelPolicies, overlayRoutes, overlayAcl, orgBadges, stats)
    views/          → 18 view modules — [3] chat, conference, rooms, files, dashboard, billing, invite, profile, auth, legal, landing ·
                          [1] remote, devices · [2] network · [·] admin, observability, analytics
    workers/        → [1] Viewer decode workers: rc-webcodecs, rc-hevc, rc-vp9-444, rc-hop-stats
    plugins/        → router, pinia, vuetify, i18n
scripts/
  dev-xvfb.sh       → Run the agent's capture path against a virtual X framebuffer (headless smoke test)
  e2e-nightly.sh · e2e-k8s.sh · signing/ · registry-retention.sh
```

### Crate dependency flow
Server spine: `config` <- `db` <- `remote_control` <- `services` <- `api`
Agent stack: `localapi` + `tcp-turn-conn` + `remote_control`(no-default) <- `tunnel-core` <- `roomler-agent`; `agent-core` <- `roomler-agent`; the tray deps only `localapi` + `agent-core`.
`tests` depends on `api` + `config` + `db` + `roomler-agent` (spawns real servers with random ports and test databases; drives the agent library in-process for end-to-end signalling, tunnel, overlay and exec tests).

## Multi-Tenancy

All data is scoped by `tenant_id`. Routes are nested: `/api/tenant/{tenant_id}/room/{room_id}/message/...`. The `tenant_members` collection tracks user-tenant membership. Room membership is tracked via `room_members`.

## Auth Pattern

JWT-based auth (jsonwebtoken 9 crate) with Argon2 password hashing:
- Access token: configurable TTL (default 604800s = 7 days)
- Refresh token: configurable TTL (default 2592000s = 30 days)
- Auth middleware extracts user from `Authorization: Bearer` header
- OAuth: Google, Facebook, GitHub, LinkedIn, Microsoft

Six `TokenType` audiences, all signed with the same JWT secret, none interchangeable:
- `Access` / `Refresh` — standard user flow
- `Enrollment` — single-use, 10 min, issued by an admin to bootstrap a new agent
- `Agent` — long-lived (1 y), carried by an enrolled daemon on its WS connection
- `TunnelEnrollment` / `TunnelClient` — the `roomler` CLI's pair (pillar 2)

Audience checks: `verify_agent_token` rejects a user JWT and vice-versa. Tests in `crates/services/src/auth/mod.rs::tests` lock this. Single-use tokens burn their `jti` into `used_tokens` (1 h TTL).

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

Route groups by pillar (full table: `docs/api.md`):
- **[3] collaboration** — auth, user, oauth, stripe/billing, invite, giphy, push, notification, tenant, member, role, room, message, reaction, recording, file, task, export, search.
- **[1] remote desktop** — `agent` (+ `agent_org`, `agent_crash`, `agent_log`, `agent_exec`), `remote_control` (sessions), `consent`, turn creds, `agent_release`.
- **[2] networking** — `tunnel` (clients + default-deny policies), `overlay_route` (nodes, approved routes, exit nodes, MagicDNS), `overlay_policy` (L3 ACL + `acl_mode`), `overlay_block` (multi-org address blocks + renumber), `tunnel_release`.
- **[·] platform/ops** — `admin`, `cluster`, `stats`/`usage` (observability), `releases`, `setup_release`, `integration`, health, `/ws`, `/derp`.

## DB Model Pattern

MongoDB native driver (not Mongoose). Models live in `crates/db/src/models/` except the remote-control / tunnel / overlay entities, which live in `crates/remote_control/src/models.rs` to keep those subsystems self-contained. **37 collections** (full ER diagrams + every index in `docs/data-model.md`):
- **[3] collaboration** — tenants, users, tenant_members, roles, rooms, room_members, messages, reactions, recordings, call_sessions, call_chat_messages, files, invites, custom_emojis, notifications, push_subscriptions, background_tasks, activation_codes, stripe_events, used_tokens
- **[1] remote desktop** — agents, remote_sessions, remote_audit, consent_requests, agent_crashes, agent_logs, exec_audit
- **[2] networking** — tunnel_clients, tunnel_policies, tunnel_audit, overlay_networks, overlay_nodes, overlay_policies, **overlay_blocks (GLOBAL, not tenant-scoped)**
- **[·] observability** — page_views, ws_sessions, audit_logs
- Indexes defined in `crates/db/src/indexes.rs`. Text indexes on messages (content), rooms (name, purpose, tags), users (display_name, username), agent_logs (lines.msg). TTLs: audit/remote/tunnel/exec audit + remote_sessions + call_sessions 90 d, agent_logs + page_views + ws_sessions 7 d, used_tokens 1 h, activation_codes/background_tasks on expiry.
- Unique composite index on `agents.{tenant_id, machine_id}` so re-enrolling a known machine reuses its row; same shape on `overlay_nodes`.
- ⚠️ `overlay_nodes`' three unique indexes are **partial** on `{deleted_at: {$type: "null"}}` — nodes are *tombstoned*, not deleted, so a released address/MagicDNS name can be re-issued while history is kept. `$type` not `{deleted_at: null}` (which also matches *absent*).
- All queries use BSON documents, no ORM. ⚠️ Hand-built `doc!{}` updates SILENTLY DROP new struct fields — confirm with `{$exists: true}` after adding one.

## Frontend Conventions

- **Plugin order**: i18n -> vuetify -> pinia -> router (in main.ts)
- **Vuetify**: Light + dark themes, auto-import tree-shaking via `vite-plugin-vuetify`
- **Stores**: Pinia with setup store pattern (`defineStore('name', () => { ... })`)
- **Rich text**: TipTap v3 with markdown support, mentions, emoji
- **WebRTC**: mediasoup client for **[3]** conferencing; a hand-rolled `RTCPeerConnection` + WebCodecs stack for **[1]** the remote viewer (`useRemoteControl.ts` + `ui/src/workers/`, five render paths — plain `<video>`, WebCodecs canvas, HEVC, VP9-444, hop-stats instrumentation). Don't confuse the two; only the conferencing one is mediasoup.
- **[2] networking UI**: `views/network/NetworkPanel.vue` (mesh + tunnels), `views/devices/`, `stores/{tunnelClients,tunnelPolicies,overlayRoutes,overlayAcl,orgBadges}.ts`; observability/analytics render the mesh graph + usage with d3.
- **API client**: `ui/src/api/client.ts` with auth token injection
- **Vite proxy**: `/api` and `/ws` proxied to `http://localhost:5001`
- **i18n**: wired, ships English only today.
- **Responsive page padding**: top-level views use `<v-container fluid class="pa-2 pa-md-4 pa-xl-6">` (8px mobile / 16px tablet+ / 24px ≥1920px). Empty-state blocks use `pa-4 pa-md-6 pa-lg-8`. Headings use `text-h5 text-md-h4` so they shrink one step on phone. Section CTAs use `size="large"` (not `x-large`) — the wider button overflows narrow viewports. Marketing/legal sections (`LandingView`, `Terms`, `Privacy`) replace fixed `py-12`/`py-16` with `py-6 py-md-12` / `py-8 py-md-16`. Custom-flex views (`ChatView`, `ConferenceView`) own their own layout and intentionally don't use `<v-container>`. Hide secondary toolbar items on `<sm` with `d-none d-sm-inline-flex`; surface a phone fallback alongside.

## Test Setup

**Integration tests** (`crates/tests/`):
- Each test gets a unique UUID-named database, auto-dropped on teardown
- Tests spawn real Axum servers on random ports
- Requires MongoDB on `localhost:27019` and Redis on `localhost:6379`
- 33 modules. **[3]** auth · tenant (+archive) · member · role · room/channel · message · reaction · recording · file · invite · notification · push · giphy · oauth · billing · multi_tenancy · pagination · rate_limit · cors · export (xlsx/pdf) · conference (+messages). **[1]** remote_control · agent (+e2e, +crash, +exec, +presence). **[2]** tunnel · overlay · relay_region. **[·]** cluster · stats. The agent-facing modules drive the real `roomler-agent` library in-process for full `rc:*` round-trips against a TestApp
- 7 known pre-existing failures on the build host (CORS tower-http upgrade ×2, role dedup, rate-limit timing, agent_e2e concurrent_sessions + terminate_clears, conference call_leave) — reproducible on pristine master and unrelated to recent work; the last three are environmental to that box

**E2E tests** (`ui/e2e/`):
- Playwright 1.58 with Chromium (fake media stream devices for WebRTC)
- 32 spec files: auth, chat (multi-client, pagination, reactions, threads, mentions), channels, rooms + files panel, conference (list/chat/multi), websocket + connection-status, dashboard, billing, invite, oauth, email flows, notifications, observability, profile, responsive, 404 — plus the remote-control lane: `remote-session-smoke`, `remote-file-upload-smoke`, `rc-vp9-444` (needs an agent built with the feature), field-host upload
- Fixtures in `ui/e2e/fixtures/test-helpers.ts`
- Base URL: `http://localhost:5000` (or E2E_BASE_URL env var); the fixtures ALSO call the API directly — set `E2E_API_URL` (defaults to the dev port `http://localhost:5001`) or every API-driven spec fails with ECONNREFUSED. Mailpit-driven specs need `E2E_MAILPIT_URL`.
- **Nightly lane** (2026-07-28): `scripts/e2e-nightly.sh` runs the whole flow below from the build host via cron (03:30 UTC) — syncs the e2e stack to the current prod tag, runs the suite, diffs failures against `scripts/e2e-expected-failures.txt`, writes `~/e2e-nightly/LATEST`, and files a GitHub issue on regressions (when `gh` is authed). Manual recipe (what the script automates; first run 2026-07-28, 142/160 passed): a standing e2e stack lives in the `roomler-ai-e2e` namespace (deploy-repo `k8s/overlays/e2e`, applied manually with `kubectl apply -k` — NOT ArgoCD-managed; bump its `newTag` in lockstep with the image you want to validate). Then `kubectl -n roomler-ai-e2e port-forward svc/roomler2 18080:80` (+ `svc/mailpit 18025:8025`), copy `ui/` to a scratch dir **minus `e2e/video/`** (the record-intro spec uses bun-only JSON-import syntax and kills collection under node), and run `docker run --rm --network host -v <scratch>:/work -w /work -e CI=1 -e E2E_BASE_URL=http://127.0.0.1:18080 -e E2E_API_URL=http://127.0.0.1:18080 mcr.microsoft.com/playwright:v$(grep @playwright/test ui/package.json | grep -oE "[0-9]+.[0-9]+.[0-9]+")-jammy bash -lc "npm i && npx playwright test --reporter=line"`. Expected environmental failures in that topology: conference specs (mediasoup RTC ports aren't forwarded), email flows (unless Mailpit is forwarded), rc-vp9-444 (needs the agent feature lane), and `oauth.spec` "clicking Google OAuth button redirects to Google" (deterministic 60 s hang: containerized Chromium intercepts `accounts.google.com` navigations before Playwright's routing/waitForRequest sees them — the identical GitHub-redirect test passes, and the server's 307 Location was verified correct directly).

**Unit tests** (`ui/src/`):
- Vitest with jsdom environment, 30 spec files
- Stores: auth, messages, rooms, ws (incl. rc:* channel), notifications, conference, tenants, files, agents, tunnel/overlay stores
- Composables: useValidation, useSnackbar, useMarkdown, useRemoteControl (HID + button mapping locks)
- API client: token injection, error handling
- Plugins: vuetify theme config

**Rust unit tests** (in-crate `#[cfg(test)] mod tests`):
- `remote_control`: locks the **wire format** — every `rc:*` tag pinned, ObjectId-as-hex, pipe-separated `Permissions` — so a rename is a deliberate break. Plus consent, session state machine, TURN creds, ACL rule matching
- `tunnel-core`: policy matching, SOCKS5 framing, mux, route descriptors; overlay internals (netmap, router, nat, disco, netstack) under `overlay-l3` / `overlay-netstack`
- `roomler-agent` lib: default features + encoder-cascade name lists (the 10 FFmpeg encoder names are test-locked), config migration, ACLs; media/input tests under the matching feature flags
- `services::auth`: token roundtrip + cross-audience rejection

**Capture smoke test** (no desktop required):
- `./scripts/dev-xvfb.sh` spins up Xvfb, paints an xterm on it, runs the scrap-capture smoke test against that virtual display. See docs in the script header for subcommands (`run`, `shell`, arbitrary pass-through).

## Environment

- `.env` — development (not committed, in .gitignore)
- Config via `ROOMLER__` prefixed env vars (double underscore separator)
- Docker: `docker-compose.yml` runs MongoDB 7 (auth credentials defined in `docker-compose.yml`; local dev only), Redis 7, MinIO, coturn
- Default DB URL: `mongodb://localhost:27019` (tests use no auth)

## Deployment

- **Production URL**: `https://roomler.ai/` — the live deployment. Use this as the `--server` argument when enrolling agents and as the origin the browser controller loads.
- **Docker**: Multi-stage build (rust:1.88-bookworm -> oven/bun:1 -> debian:trixie-slim + nginx)
- **Deploy repo**: `<deploy-repo>` on the build host. Kustomize manifests live under `k8s/base/` + `k8s/overlays/prod/`. Ansible playbooks retained for host-level tasks only (HAProxy, WireGuard, iptables).
- **GitOps**: ArgoCD (at `<argocd-host>`) reconciles the `roomler-ai` Application from the deploy repo's `master` branch, path `k8s/overlays/prod`. Sync policy is **Automated + selfHeal + prune** with a GitHub webhook on the deploy repo: `git push` to master rolls out within ~5 s. 60 s polling fallback via `argocd-cm.timeout.reconciliation: 60s`. Sibling Application CRDs (bauleiter / lgr / oxmux / purestat / regal / roomler-ai / roomler-old / tickytack) are gitops-managed under a parent app-of-apps. Verify the live targetRevision with `argocd app get roomler-ai --grpc-web | grep -E "Target|Sync Status"`.
- **Image registry**: `<internal-registry>` (self-hosted Docker Registry v2 on the build host, basic auth, cert auto-renewed via acme.sh). Pull secret `regcred` lives in the `roomler-ai` namespace.
- **K8s cluster**: 3 control-plane + 3 worker nodes (Ubuntu 22.04, containerd 1.7.29, v1.31.14). Three zones via `topology.kubernetes.io/zone` (one master + one worker VM per bare-metal host).
- **Tier policy** (added 2026-05-01): cluster nodes are labelled `tier=high-performance` (the two high-perf worker hosts) and `tier=utility` (the build/utility worker host). roomler-ai schedules on `tier=high-performance` only — never on the utility worker. Enforced via a Kustomize patch in `<deploy-repo>/k8s/overlays/prod/kustomization.yaml` (commit `dab3cfa`) that adds a required `nodeAffinity` to every Deployment + StatefulSet. Hostname pin in `base/` (`kubernetes.io/hostname: <storage-pinned-worker>`) is intentionally retained — the StatefulSet PVCs use node-local storage, so the data lives on that specific node; the tier requirement is an *additional* constraint, both must match. **Utility worker hosts**: monitoring (kube-prometheus), `<internal-registry>`, image builds (direct on the host), `bauleiter`, `regal`. **High-perf workers**: roomler (old), roomler-ai, oxmux, clawui (when migrated to K8s), lgr, purestat, tickytack.
- **Pod placement** (S6, 2026-07-28): the API runs **2 replicas**, one per high-performance worker, forced apart by a `podAntiAffinity` overlay patch (hostNetwork ports collide on a shared node). The API Deployment's hostname pin was dropped; the Mongo/MinIO StatefulSets keep theirs (node-local storage). Namespace `roomler-ai`, deployment `roomler2` (note: name is `roomler2` not `roomler-ai`), **RollingUpdate maxSurge 0 / maxUnavailable 1** (zero-downtime deploys; surge 0 because hostNetwork can't double-bind), hostNetwork, `imagePullPolicy: IfNotPresent`. Each pod resolves its own public mediasoup announced IP from `ROOMLER__MEDIASOUP__ANNOUNCED_IP_MAP` (`<node_ip>=<public_ip>,...`) keyed by the Downward-API `ROOMLER__POD_HOST_IP` (status.hostIP); the static `announced_ip` is the fallback.
- **Tenant-affinity LB** (S6, LIVE 2-pod since 2026-08-02): the front reverse proxy (docker-nginx on the build host) routes to the pods through a `hash <tenant-key> consistent` upstream — key = path tenant (`/api/tenant/{tid}/...`) → `?tid=` query param → client IP. This co-locates a tenant's users, agents, tunnel clients, DERP sockets and mediasoup rooms on ONE pod (the rc-hub / tunnel-hub / DERP relay / room registry are pod-local). **Strict pinning for long-lived sockets**: the upstream runs `max_fails=0` (the balancer must never mark a peer unavailable — with consistent hashing that walks the ring to the survivor, and a WS that reconnects during a deploy roll PARKS on the wrong pod after recovery, splitting controllers from agents), `location = /ws` + `location = /derp` add `proxy_next_upstream off` (a rolling pod ⇒ fail fast, client backoff re-homes correctly), and plain HTTP keeps per-request failover via `proxy_next_upstream error timeout` + a 5 s connect timeout. To park a pod (fall back to single-pod): add ` down` to its server line + `nginx -t && nginx -s reload`; **after ANY flip either way, also `kubectl rollout restart deploy/roomler2`** so long-lived WSs re-hash. Cross-pod chat/notifications/presence ride the Redis fan-out; a Redis online-registry (`roomler:online:<uid>`, 90 s TTL + 30 s heartbeat) backs the offline push/email dedupe; startup maintenance (stale-call reset, migrations) is leader-gated behind a 120 s Mongo lease (`locks` collection). ⚠️ Lesson (2026-07-29 + 2026-08-02 incidents): the listing's `is_online` is heartbeat-based (HTTP) while rc/tunnel need the pod-local hub — an agent whose control WS is half-open (a TLS-inspecting corp middlebox keeps ACKing its keepalive pings after a pod roll killed the upstream leg) shows GREEN but is `agent_offline`; agents ≥rc.293 detect this via a receive-liveness deadline (no inbound frames for 80 s ⇒ reconnect) and self-heal in ≤~2 min.
- **Health probes**: startup/readiness/liveness all on `/health` (port 80 via nginx -> :3000 backend)
- **nginx**: Pod-internal reverse proxy (`files/nginx-pod.conf`) — SPA fallback + API proxy + WS proxy
- **Agent binary**: built separately (`cargo build -p roomler-agent --release --features full`) and distributed to controlled hosts via GitHub Releases (MSI / .pkg / .deb auto-built by `.github/workflows/release-agent.yml` on `agent-v*` tag push). Not part of the API Docker image.

### K8s deploy pipeline (ArgoCD GitOps)

The build host builds the image, pushes to `<internal-registry>/roomler-ai:<tag>`, bumps the tag in the gitops repo, and ArgoCD reconciles the Deployment. Fill in the env vars at the top once per shell session:

```bash
# Operator-filled (set once per shell):
: "${BUILD_HOST:=ssh-target}"            # e.g. your build host alias
: "${REGISTRY:=registry.example.com}"    # your <internal-registry>
: "${REPO:=$HOME/roomler-ai}"            # local clone of this repo on the build host
: "${DEPLOY_REPO:=$HOME/roomler-ai-deploy}"

ssh "$BUILD_HOST"
cd "$REPO" && git pull
docker build -t "$REGISTRY/roomler-ai:build-$$" .                   # ~5–15 min (cache warm)
TAG="v$(date +%Y%m%d)-$(docker images -q "$REGISTRY/roomler-ai:build-$$" | head -c 12)"
docker tag "$REGISTRY/roomler-ai:build-$$" "$REGISTRY/roomler-ai:$TAG"
docker tag "$REGISTRY/roomler-ai:build-$$" "$REGISTRY/roomler-ai:latest"
docker push "$REGISTRY/roomler-ai:$TAG"
docker push "$REGISTRY/roomler-ai:latest"

# ── ALWAYS run after every deploy: reclaim the build's disk footprint. ──
# The image is safely in the registry now, so the local copies are just build
# leftovers. Every deploy bakes a fresh multi-stage image (+ intermediate layers
# + build cache); without pruning they pile up until the build host's root FS
# fills. (2026-07-12: `/` hit 100% from ~13 GB of stale build images mid-deploy.)
# `-a` drops images not backed by a RUNNING container, so the mongo + registry
# containers (and their images) are untouched; NO `--volumes`, so mongo DATA is
# safe. Reclaims the per-deploy delta every time.
docker system prune -af
docker builder prune -f
df -h / | awk 'NR==2{print "build-host / : "$4" free ("$5")"}'   # sanity

cd "$DEPLOY_REPO"
git checkout master && git pull
sed -i "s|newTag:.*|newTag: $TAG|" k8s/overlays/prod/kustomization.yaml
git commit -am "chore(k8s): bump roomler-ai to $TAG"
git push

argocd app sync roomler-ai --grpc-web     # or Sync via the ArgoCD UI
curl -sI https://roomler.ai/health        # HTTP/2 200
```

Registry retention: `registry-retention.sh 1` (weekly cron at Sun 04:00) keeps at most 2 tags per repo (latest + most-recent-versioned) and GC's the registry storage. **Run it manually if `/gjovanov/registry` is fat** — the blob store isn't touched by `docker system prune` (it's the registry's own storage, not docker's), and heavy repos (e.g. `lgr` at ~7.5 GB/image) accrete fast between weekly GCs.

**Periodic build-host maintenance (NOT per-deploy):** the fattest reclaimables are the local Rust `target/` dirs of the *other* projects cloned on the build host (`~/{harvex,oxmux,purestat,parakeet-rs}/target` were ~44 GB combined on 2026-07-12) — `cargo clean` or `rm -rf <proj>/target` when idle; they just recompile on next build. **Never touch `/var/lib/libvirt`** (the running k8s master+worker VM disks, ~87 GB) or the active container data volumes.

## Post-Implementation Testing

After every feature or fix, verify your changes:

| Change type | Command | What it checks |
|-------------|---------|----------------|
| Backend (models, services, routes) | `cargo test -p roomler-ai-tests` | Integration tests (real MongoDB) |
| Remote-control crate (Hub, signalling, wire format) | `cargo test -p roomler-ai-remote-control --lib` | Unit tests (no MongoDB required) |
| **[2]** Tunnel / transport / policy | `cargo test -p roomler-ai-tunnel-core --lib` | Forwards, SOCKS5, mux, ACL matching |
| **[2]** Overlay mesh | `cargo test -p roomler-ai-tunnel-core --lib --features overlay-l3` (and `overlay-netstack`) | ⚠️ WITHOUT the feature the overlay tests are silently SKIPPED |
| Agent library | `cargo test -p roomler-agent --lib` | Default-feature unit tests |
| Agent with media / input backends | `cargo test -p roomler-agent --lib --features full` | Needs libxcb*-dev + libasound2-dev on Linux |
| Agent capture against a headless display | `./scripts/dev-xvfb.sh` | Xvfb + xterm + capture smoke test |
| **[1]** Encoder cascade on real silicon | `roomlerd encoder-smoke --encoder hardware [--codec hevc]` | 10 synthetic frames; prints every cascade decision |
| Frontend (views, stores, composables) | `cd ui && bun run build` | TypeScript + Vite build |
| Frontend unit tests | `cd ui && bun run test:unit` | Vitest (30 spec files) |
| Full-flow (auth, routes, UI+API) | `cd ui && bun run e2e` | Playwright E2E tests |

Run the **most specific** command first. If a backend change also affects the frontend, run both.

⚠️ **Networking changes are not done when CI is green.** Pillar 2 lives or dies on real topologies — field-validate on the fleet (corp-VPN laptop, corp-firewall host, Linux server, WSL, dev box) via `roomler exec` after every roll. The `tunnel-fleet-test` skill drives a real forward through a real agent end-to-end.

### Defensive enum catch-alls

`ClientMsg` / `ServerMsg` in `crates/remote_control/src/signaling.rs` are matched exhaustively from multiple consumer crates (agent, api/ws, hub). When you preemptively add a `_ =>` / `other =>` catch-all arm in a consumer match **without** adding the new variants that would make it reachable in the same commit, `cargo clippy --workspace -- -D warnings` fails with `unreachable_patterns` (the existing arms already cover every known variant). CI run [25972574628](https://github.com/gjovanov/roomler-ai/actions/runs/25972574628) hit this — defensive catch-all landed in `ec61f03` before the corresponding T2 wire variants did. The rule:

- If the new variants are landing in **the same commit**: no allow needed, the catch-all is immediately reachable.
- If the new variants are landing in **a later commit** (defensive future-proofing): annotate the catch-all with `#[allow(unreachable_patterns)]` and reference this rule in a comment so the next reviewer doesn't strip the allow. Remove the allow when the variants land.
- `#[non_exhaustive]` on the enum upstream is the structural alternative but forces a catch-all in every consumer everywhere — too invasive for the existing `signaling::*` matches.

## Pillar 1 — Remote Desktop (TeamViewer-class)

One native daemon per controlled host, Roomler API as a signalling-only relay, browser as controller. All media + input flows over direct WebRTC P2P (TURN-relayed if needed) — **the server never sees raw pixels or keystrokes**.

**Docs**: `docs/README.md` is the navigable index of every doc (three-pillar split). Deep-dive: `docs/remote-control.md` (19 sections; §17-19 are historical appendices) and `docs/encoders.md` (the current encoder reference: codec × platform × backend matrix, HW cascade, rate control, capture backends, viewer decode paths).

**Capabilities shipped**: multi-monitor capture (DXGI / WGC / X11 / macOS) → HW encode (MF, NVENC/QSV/AMF via FFmpeg, openh264/VP9-444 SW) → **two video transports** (WebRTC RTP *and* a reliable DataChannel + WebCodecs canvas path that bypasses Chrome's ~80 ms `<video>` jitter floor) · input injection with keyboard-layout auto-switch, keyboard lock in fullscreen and a Ctrl+Alt+End SAS chord · clipboard v2 (text/image/HTML/RTF auto-sync via a local loopback bridge) · resumable file transfer · remote app launch · system audio · cursor overlay · Windows pre-logon **SystemContext** control (lock screen, UAC, elevated apps) · viewer indicator · per-hop viewer instrumentation.

**Wire protocol**: `rc:*` JSON messages over the existing `/ws` endpoint. `ClientMsg` / `ServerMsg` in `crates/remote_control/src/signaling.rs`. ObjectIds are raw hex strings (locked by tests); `Permissions` serialises as pipe-separated names (bitflags 2.x convention, also locked).

**WebSocket role multiplexing**: `/ws?token=<jwt>&role=agent` uses the agent JWT audience; no `role` param (or `role=user`) uses the existing user flow. Same WS endpoint, same handshake, different claim validator. **Tenant affinity (S6)**: `/ws` (all roles) and `/derp` also accept an optional `tid=<tenant-hex>` the front LB hashes on — agent/tunnel tokens must match their `tenant_id` claim; user tokens are checked against `tenant_members` (403 on a non-member claim); absent `tid` = legacy client, accepted.

## Pillar 2 — Networking: overlay mesh (Tailscale-class) + tunnels (ngrok-class)

Read the **design goal** at the top of this file before changing anything here — it is the acceptance bar for every decision below.

Two halves of one networking product, both served by `crates/tunnel-core` and hosted in `roomlerd`:

- **Overlay mesh (L3, Tailscale-class)** — a private WireGuard network (userspace boringtun, `100.64.0.0/10` CGNAT range, MTU 1280) between all enrolled machines of a tenant. Server-issued **netmaps** are the *only* introduction mechanism (no on-wire peer discovery). Features: MagicDNS, subnet routers (advertised → admin-approved routes), **exit nodes** (full `0.0.0.0/0` egress), L3 ACLs with per-source ingress rules, dual-stack (v6 derived from v4, never separately allocated), multi-org membership, and a **userspace netstack mode** (loopback SOCKS5 front, no OS TUN/routes) for hosts where the routing table isn't ours to take.
- **Tunnels (userspace, ngrok-class)** — on-demand `host:port` reachability without joining the mesh: TCP forwards, SOCKS5 with CONNECT **and** UDP ASSOCIATE (per-agent or one tenant-wide mesh proxy addressed by agent name / LAN longest-prefix), and daemon-supervised **declared routes**. Data plane is QUIC (`quic-v1`, preferred) or a WebRTC DataChannel pool (`webrtc-dc-v1`), each climbing its own relay ladder; when both ends are in the mesh, flows can ride the WireGuard plane instead.
- **Roomler SSH** (P1+P2, `ssh-server` feature, default-off) — SSH to any node by its overlay address with no `sshd`, no bound port, no key distribution: packets are intercepted below the OS by `split_tun` into the in-process smoltcp netstack. Detail below.

**Carrier cascade** (the heart of it): LAN → direct-public → srflx hole-punch → single-relay TURN (QUIC-upgraded; UDP 3478 → TURNS TCP/UDP :443) → **DERP over WSS :443** as the always-on floor. Selection is a **server verdict computed from measured CapVectors**, not a client heuristic; heuristics DETECT, the server DECIDES. Never ratchet — a relayed pair keeps re-attempting direct (make-before-break, then relentless re-upgrade).

**Docs**: **`docs/overlay-communication.md` — start here** (control plane vs data plane, every carrier tier as a sequence diagram, which carrier wins inside vs outside a corporate VPN, with field-proof). Then `docs/overlay-nat-traversal.md` (cascade mechanics, NAT probing, cooldowns, PathMonitor), `docs/tunnels.md` (concepts + protocol + LocalAPI + CLI), `docs/tunnel-install.md` (runbook), `docs/agent-tunnel-architecture.md` (5-minute overview), `docs/overlay-wfp.md` (Windows Filtering Platform override for GPO-locked firewalls), `docs/overlay-exit-nodes.md`, `docs/multi-org.md` (one device in N orgs + the block-renumber runbook + failure matrix), `docs/fleet-rpc.md`, `docs/roomler-ssh.md`. In-flight design records: `docs/overlay-session-proof.md`, `docs/overlay-warm-relay.md`, `docs/overlay-symmetric-punch.md`.

⚠️ **Windows route war** — Windows ranks `route metric + INTERFACE metric`, then lower ifIndex. A corp-VPN adapter mirroring our routes at 1+1 makes an exact tie it wins stickily per destination. `Find-NetRoute -RemoteIPAddress <peer>` names the winning interface in one command — use it before theorising about one-way carriers (replies escape via strong-host, so the far side reports healthy RTT while traffic is captured). The lever is pinning the TUN **interface** metric to 0; metric-0 *routes* can be deleted, an interface metric can't.

**Multi-org** (`docs/multi-org.md`): ONE multi-tenant daemon, never N side-by-side installs (fixed TUN name+GUID, LocalAPI pipe singleton, host-global exit/DNS/WFP, per-machine updater). The config's scalar identity stays the PRIMARY enrollment (rollback-safe); secondaries live in `[[orgs]]` with their OWN freshly-minted WG key (never a copy — cross-org pubkey correlation), one supervised WS loop each, per-org `DOWN_SINCE`, and `rc:agent.update` honored ONLY from the primary. Overlay addressing: every legacy tenant shares `100.64.0.0/10` seeded at `.1`, so tenant A's and tenant B's `100.64.0.7` are the SAME address — **P2b** carves disjoint blocks from a GLOBAL `overlay_blocks` registry (aligned `/22` slots, monotonic from slot 64 = `100.65.0.0`; the whole `100.64.0.0/16` below is the legacy reserve). Non-overlap is structural: `slot` is uniquely indexed and starts are buddy-aligned, so concurrent allocations either collide on one slot (index arbitrates) or are disjoint — no lock. Freed blocks are **quarantined, never re-issued** (a device that missed the migration still believes it holds an address there). Carving is behind `ROOMLER__OVERLAY__BLOCKS_ENABLED` (default OFF ⇒ zero registry reads, pre-P2b behaviour) and only ever touches VIRGIN networks; an existing tenant moves via `POST …/overlay-block/renumber`, **dry-run by default**, which preserves ordinals where they fit, gates on `ROOMLER__OVERLAY__BLOCK_VERSION_FLOOR` (rc.301 = the P2a forward-compat set; below it a daemon purges its own on-link route at boot ⇒ host-wide blackhole) and then CYCLES every agent WS — `self_ip` binds once at establish, so nothing else makes a live fleet re-bind. ⚠️ The cycle is disruptive: a corp-VPN host can come back relay-locked. ⚠️ Tunnel-client nodes have no server-side cycle primitive — they're reported as `reconnect_required`. **P2c (agent-side shared TUN)**: `tunnel_core::overlay::tun_mux` — ONE `roomler` adapter, per-org `MuxPort` facades, dst-based longest-prefix demux whose table is built from the runtime's own route installs (`add_peer_route` /32s + `add_cidr_route` subnet/exit routes + the org's block from registration) so OS and demux tables can't drift; derived-ULA v6 unmaps to embedded v4. Behind the agent config key `overlay_multi_org` (default OFF ⇒ P1 behaviour byte-for-byte); the `for_org` gate additionally requires `overlay_mode="tun"` + the PRIMARY's `server_url` + the org's own WG key. One legacy `/10` org coexists with carved blocks (longest prefix); a SECOND un-migrated `/10` is refused at registration (`AddrInUse`) — renumber one. Exit roles + netstack stay primary-only/overridden; macOS has no multi-address TUN (`SystemTun::add_address_sync` refuses). **Mux NAT (rc.328, `docs/multi-org.md` §4b)**: the OS can pick the WRONG org's source for a shared-adapter destination (nested blocks defeat source selection — field 2026-08-09, 100 % loss toward single-org peers); `overlay/mux_nat.rs` + `tun_mux` hooks normalize cross-org egress sources and restore reply destinations (kill switch `overlay_mux_nat`, default on), Linux routes additionally carry `src` hints, and receivers split the signature into `rx_denied_noroute` (`peers --json`). **Multi-org v2 (rc.333-339, `docs/multi-org.md` §4a)**: `overlay_shared_carrier` (ONE process-wide direct-socket set, receiver-index demux) + `overlay_tun_per_org` (per-org adapters; primary keeps `roomler`/`roomler0`) are **default ON since rc.339** after the 4-host soak — explicit `false` per key is the kill switch; the mux/NAT/SkipAsSource stack above is the flag-OFF fallback until its evidence-gated deletion (counters in `peers --json` must stay fleet-zero).

**Overlay exit nodes** (`overlay-l3`, default-OFF): a client routes its whole internet egress (`0.0.0.0/0` + `::/0`) through a chosen mesh peer. Config keys: exit offers with `overlay_exit_node_enabled=true`; an admin approves via `PUT …/overlay-node/{id}/exit-node` (writes `is_exit_node` + adds `/0` to `approved_routes` — the data-plane signal, NOT `is_exit_node` alone); a client opts in with `overlay_exit_node="<name|hex>"`. Core invariant = **never self-wedge**: pin `/32`/`/128` carrier+control exemptions first, then install the split-default (`0.0.0.0/1`+`128.0.0.0/1`; v6 `::/1`+`8000::/1`), else WITHHOLD; route-guard re-asserts every 2 s; boot-reconciler + `purge_exit_routes()` heal a stale `/1` after a hard exit. DNS steered to the exit's vantage (no leak). ⚠️ An exit reroutes the host's own *inbound*-reply traffic → it breaks un-exempted SSH; and NEVER run the exit field-test on a prod cluster node (see `docs/overlay-exit-nodes.md` caveats). Full detail in `docs/overlay-exit-nodes.md`.

**Overlay address leases — allocation + RELEASE**: `overlay_networks` holds a monotonic `next_host` cursor **and** `free_hosts`, a pool of recycled host numbers. `allocate_host` pops the pool head (FIFO) before bumping the cursor; both branches are single atomic `find_one_and_update`s so concurrent joiners can't collide. v6 is DERIVED from the v4 (`derive_overlay_v6`) — one allocator, freeing v4 frees both. All three removal paths (`DELETE …/agent/{id}` cascade, `DELETE …/overlay-node/{id}` admin evict, `DELETE …/tunnel-client/{id}`) funnel through `ws::overlay::release_overlay_node`, whose **order is load-bearing**: read peers while live → **CAS-tombstone** (winning the CAS is the release *token*, so two concurrent removals can't pool one host twice) → pool the host → fan `netmap_delta{removes}` to peers *and* to the released node. Pooling BEFORE the tombstone would hand the address out while the old row still held it and the unique index would lock that joiner out permanently; this order only ever leaks a host. Rows are **tombstoned, not deleted** (address/name/pubkey kept as the record of who held them) and the three unique indexes on `overlay_nodes` are `index_unique_partial(..., {deleted_at: {$type: "null"}})` so a tombstone holds neither address nor MagicDNS name — `$type` not `{deleted_at: null}`, which also matches *absent*. `find_live_by_tenant_and_machine` is live-scoped ⇒ **removal is final**: a re-enrolled machine gets a fresh lease, never the revived tombstone. Evict = "force a new lease", NOT a ban (a still-enrolled device rejoins with a different address; there is no per-machine denylist). Client-side consequences: routing teardown is keyed by **pubkey** (`Router::remove_by_pubkey`; the IP-keyed variant is gone), an OS `/32` is dropped only when no surviving peer claims it, `install_peers` reinstalls on a pubkey rotation, and an `overlay_exit_node` **name** is pinned to the node it first resolved to. Those fire on the delta `removes` arm and `sweep_carrier_health`'s lazy reap — the two paths that can reap a stale peer *after* its address was recycled. ⚠️ The full-netmap arm also diff-and-prunes, but that is **defensive only and unreachable today** (field-checked 2026-08-02): `run()` joins once and eats the first netmap before the loop, the server only sends a full netmap in reply to a join, and the runtime is scoped to ONE WS session — a disconnect drops `by_node`/WgDevice/TUN and the reconnect rebuilds from empty, so there is no "stale peer survives a disconnect" leak to fix. Don't cite that arm as a live protection. ⚠️ **Pre-release gaps are ACCEPTED, not a bug**: devices removed before 2026-07-29 burned their host number with nothing to return it, so a long-lived tenant's `next_host` sits above its live count with holes below (fleet tenant: cursor 33, 14 live, 16 orphans `.3 .5 .8–.13 .16–.23`). They belong to NO document, so eviction can't reach them, and reclaiming means writing `free_hosts` directly — bypassing every `release_host` guard for a 4.19 M-address `/10`. Leave them; if it ever matters, build an admin reconcile that routes gaps back through `release_host`. Detail in `docs/overlay-communication.md` §1.

**Declared tunnel routes (P6)** — `roomlerd` supervises forwards/SOCKS5 listeners declared in its config (`[[tunnel_routes]]`, `tunnel_core::localapi::RouteDescriptor` = one type for wire + disk): `agents/roomler-agent/src/tunnel/route_reconciler.rs` reconciles them into hub flows on every start (create-retry backoff; terminal `failed` on revoked/cross-tenant so a dead route never hammers the server), `roomler route add/rm/ls/enable/disable` + the desktop Tunnels section manage them over the LocalAPI `Route*` verbs, and the daemon persists through an atomic `config::save` + a daemon-wide write lock. See `docs/tunnel-install.md` §6 "Declared routes".

**Tunnel policy — two independent gates**: (1) server-side `tunnel_policies`, **default-deny**, evaluated per flow open as *subject* (user / tunnel client) × *target* (agent) × *destination* (`host:port`, protocol), every decision audited in `tunnel_audit` (90 d); (2) the exit machine's own `forward_acl` in config.toml — the last word, and the only refusal that survives a compromised server (empty-but-enabled means "trust the server"). ⚠️ A `cidr` rule NEVER matches a SOCKS5 **hostname** target.

**LocalAPI** — the tokenless on-host control surface shared by `roomler`, `roomler-desktop` and scripts: Windows named pipe `\\.\pipe\roomler` (SYSTEM + Admins + Interactive Users, no-write-up), Unix socket `$XDG_RUNTIME_DIR/roomler.sock` (0600); newline-delimited JSON `{"t": "<verb>", "d": {…}}`. Verbs: `Status` · `Peers` · `Flows` · `Ping` · `CreateForward` · `CreateSocks5` · `KillFlow` · `Route*` · `Consent*` · `SetDeviceName`. Protocol reference in `docs/tunnels.md`.

**Roomler SSH (P1+P2, 2026-08-19)** — SSH into any enrolled node by its overlay address with **no `sshd`, no bound port, no firewall rule**; the roomler answer to Tailscale SSH, and unlike theirs it can serve Windows. The packets are **intercepted below the OS**: `tunnel_core::overlay::split_tun::SplitTun` is a `TunIo` spliced between the WG bridge and the real device that diverts TCP for `<self overlay ip>:<ssh_port>` into the in-process smoltcp netstack and passes everything else through. That is not an elegance choice — field-measured 2026-08-19, binding `overlay:22` FAILS with EADDRINUSE on mars/zeus/jupiter (sshd on `0.0.0.0:22` covers every local address, in BOTH orgs) and on neo16 (sshd bound to the overlay IP itself), while clk00017265 cannot have sshd at all (capability `NotPresent`, corp-managed, WSL holds loopback `:22`, all 3 firewall profiles on). Interception also leaves nothing for an EDR agent to terminate — the failure that parked `regal` outbound-only when Kaspersky killed `sshd.exe` as a service — and makes "unreachable off-mesh" a property of the topology rather than a policy. Server = russh 0.62 (`default-features=false, features=["ring"]` — **defaults would pull aws-lc-rs**, a C/NASM build that breaks tunnel-core's ring-only invariant; `rsa` deliberately off), behind the `ssh-server` cargo feature: **+1.86 MiB measured in roomlerd (29.94→31.80, +6.2%; ~+0.7 MiB on the MSI), ~99 crates, a second RustCrypto generation** (russh is on aes-gcm 0.11 / curve25519-dalek 5 / p256 0.14 vs our 0.10/4/0.13, so nothing is shared) — hence opt-in per build and NOT yet in the release feature sets. Config: `ssh_enabled` (default off), `ssh_port` (default **2222**, not 22, so an existing sshd keeps serving the overlay address during migration — the daemon warns when it shadows one), `ssh_authorized_keys` (empty = nobody, so `ssh_enabled` alone grants nothing), `ssh_host_key` (ed25519, minted on first SSH-enabled start, stored in config.toml so it inherits the atomic+fsync+`.prev`+0600/ACL treatment; **if it cannot be persisted SSH stays OFF** rather than serving a per-boot identity). `exec` runs through the existing `crate::exec` engine, inheriting its timeout / output ceiling / concurrency cap / redaction / process-tree kill; PTY, shell and SFTP are refused **with a reason on stderr** (a bare channel failure is why `scp` would otherwise just hang). ⚠️ Sessions inherit the daemon identity (SYSTEM/root) — privilege drop is P5, so a listed key is root today. **All four gates are live** (P3a+P3b): carrier identity → org `remote_ssh_enabled` (a SEPARATE switch from `remote_exec_enabled`) → `SSH_DEVICE` (1<<29, a SEPARATE bit from `EXEC_DEVICE`, NOT in `DEFAULT_ADMIN`) → per-device `SshPolicy` → the two device-owned config keys. Decision point `crates/api/src/routes/agent_ssh.rs`; both the HTTP route and the `rc:ssh.request` device leg go through ONE `dispatch`. The server mints a single-use grant (ephemeral pubkey + principal + account mode + 60 s expiry), pushes it to the target and answers the caller with where to dial — its role ENDS there, because the session rides a path it never observes, which is also why every refusal reason is enumerated and answered synchronously. Agent re-clamps the grant against its OWN clock (server timestamps can only shorten), single-use, table capped at 16. Full design + roadmap in `docs/roomler-ssh.md`.

## Pillar 3 — Collaboration (chat · conferencing · teamwork)

The original product surface, and the one the multi-tenancy / auth / route / DB patterns above were built for. All of it is server-mediated (unlike pillars 1 and 2) — this is the one plane where media traverses Roomler infrastructure.

- **Rooms** — hierarchical tree of text and voice/video channels (`parent_id`, unique `(tenant_id, path)`, sparse-unique `meeting_code`), per-room membership, roles with a 24-bit permission bitfield.
- **Chat** — threaded messages, reactions (unicode + tenant custom emoji), mentions, pins, embeds, Giphy, TipTap v3 rich text with markdown, full-text search over a Mongo text index, pagination.
- **Conferencing** — mediasoup SFU running **in-process** (`crates/services/src/media/`: `worker_pool`, `room_manager`, `signaling`), active-speaker detection, layouts, PiP, in-call chat, recordings to MinIO/S3. Each pod resolves its own public announced IP from `ROOMLER__MEDIASOUP__ANNOUNCED_IP_MAP` keyed by the Downward-API host IP; the scale ladder is settled in `docs/multi-pod-scale-out.md`.
- **Around it** — files (versioned uploads), invites (shareable/email/batch), notifications + web push (VAPID), email flows, exports (xlsx/pdf), Stripe billing, OAuth (Google, Facebook, GitHub, LinkedIn, Microsoft).
- Real-time delivery is pod-local WebSocket sessions fanned out across pods via **Redis pub/sub**; a Redis online-registry (`roomler:online:<uid>`, 90 s TTL + 30 s heartbeat) backs offline push/email dedupe. Surfaces documented in `docs/real-time.md`; the frontend map is `docs/ui.md`.

## Node stack — packaging, installer & fleet ops

Cross-cutting machinery that serves pillars 1 and 2 on every enrolled machine. Install paths and service modes: `docs/installation.md`.

**Unified installer (P4, 2026-07-17)** — ONE wizard for the whole node stack:
- **`agents/roomler-setup/`** (Tauri 2 single-window app, lib `wizard_app`) + **`crates/roomler-setup-core/`** (event-shape-free mechanics, lib `wizard_shared`). Role picker on Welcome: three daemon flavours on Windows (perMachine-SCM service / perUser task / perMachine attended — mapping to the MSI flavours) + tunnel-client on any OS. Steps: Welcome/role → Server → Token → Install → Done, with cancel/force-kill, progress replay, cross-flavour ack gate, and wizard-state persistence (**token NEVER persisted**). Daemon roles also place the `roomler-desktop` companion EXE (GAP-A). Released by `release-setup.yml` on `setup-v*` tags (Linux/macOS tarballs + SIGNED Windows EXE in `.zip`); first field-proven at setup-v0.3.0-rc.197.
- **Backend proxies** in `crates/api`: `/api/setup/{latest-release,{platform}/health,{platform}}` serves the wizard itself (routes/setup_release.rs) + `/api/setup/install.{sh,ps1}` serve the terminal (no-GUI) installers embedded at compile time from `scripts/`. `/api/agent/installer/{flavour}` + `/health` (routes/agent_release.rs) streams MSI bytes through `roomler.ai` (NOT `github.com`) so corporate ESET / Defender allow-lists trust the download; `/api/tunnel/installer/{platform}` serves the CLI tarball.
- **UAC lib-naming rule** — Windows UAC's "installer detection" heuristic auto-elevates any EXE whose filename contains "install" / "setup" / "update" / "patch"; cargo derives test-binary names from the LIB crate, so wizard lib targets must dodge those substrings (`wizard_app`, `wizard_shared`; historically `wizard_core` / `tunnel_wizard_core`). The user-facing bin EXE keeps the marketing name; `[[bin]] test = false` keeps `cargo test -p roomler-setup` off the UAC prompt.
- **Legacy wizards RETIRED in P4c-2** — `agents/roomler-installer` (rc.28 agent wizard) and `agents/roomler-tunnel-installer` (rc.59 tunnel wizard) were reduced to shims over `wizard_shared` in P4a and deleted after the unified wizard's field-proof, along with `release-tunnel-wizard.yml`, the installer-EXE half of release-agent.yml's companions job, and the legacy `/api/tunnel-wizard/*` route family. The tunnel CLI's `self-update` is KEPT — it's the sole updater for tunnel-only hosts ("one updater" is per-role; daemon hosts get `roomler.exe` refreshed by the MSI).

**Install-size trim (P3e, 2026-08-15)** — the per-Machine install was 102.6 MiB on disk (`roomlerd.exe` 61.7 + `roomler.exe` 22.1 + `roomler-desktop.exe` 16.5 + 1.9 CRT + wintun), MSI 31.97 MiB. Two levers landed:
- **Lever D — `roomler.exe` is a shim.** The command surface moved from `roomler-tunnel`'s `main.rs` into its LIB (`roomler_tunnel::cli`, entry `run_from(argv, Origin)`); `roomlerd cli <args>` dispatches into it from the FIRST statement of `main()` (raw-argv check, deliberately ahead of DPI awareness / the 1 ms timer / the legacy-tree migration / `logging::init` — a CLI call must not run daemon-startup side effects), and both MSIs now install the ~150 KB `roomler-cli-shim` under the unchanged name `roomler.exe`. Rationale: `cargo bloat` put only 1.16 MiB of that 22.1 MiB binary in CLI-specific code — the rest duplicated std/webrtc/tunnel_core/tokio/rustls/reqwest/quinn/clap that `roomlerd.exe` already links — and the MSI job spent a serial 143 s rebuilding it under a different feature set. `self-update` REFUSES under `Origin::EmbeddedInDaemon` (the MSI owns the whole node stack; without the guard a daemon host would swap its own shim for a 22 MiB CLI). Tunnel-only hosts are untouched: `release-tunnel.yml` still ships the real standalone binary. ⚠️ The Linux .deb still ships the full `roomler` CLI — same duplication, same fix, but the shim needs SIGINT ignoring on Unix first or Ctrl-C orphans the child.
- **Lever H — CRT trim.** The `VcRedistCrt` component shipped all nine Microsoft.VC14x.CRT DLLs; the import tables want three (`msvcp140`, `vcruntime140`, `vcruntime140_1` — and msvcp140 itself imports only the two vcruntimes). The other six are satellites a binary imports DIRECTLY when it uses the matching STL feature; none of ours does. Re-check with `dumpbin -dependents <exe> | findstr /i 140` before adding a C++ dependency, and extend the wxs components AND both workflows' staging lists together.
- **Lever E — the desktop companion stopped linking the daemon.** Two leaf crates: `crates/localapi` (the LocalAPI protocol, re-exported as `tunnel_core::localapi` so every call site is unchanged) and `crates/agent-core` (config/config_surface/enrollment/machine/logging/logs_upload/crash_recorder/notify-primitives/acl/apps-config, re-exported by `roomler-agent` under the old `crate::` paths). `dst_matches`/`host_matches` moved to `remote_control::models` next to their canonical shapes (tunnel-core `policy` re-exports). The tray's graph went **470 → 368 crates with ZERO transport crates** (webrtc family, quinn, turn, tokio-tungstenite, openh264, scrap, enigo, async_zip all gone). Seams to know: the rc.53 worker-aware notify trio stays in `roomler-agent/src/notify.rs` (probes SystemContext); `apps` re-exports the moved config shapes; `appdirs::service_log_dir()` is the canonical SCM-log path (win_service delegates); `config::test_fixture` is behind the `test-fixtures` feature for downstream test builds (cfg(test) doesn't cross crates); agent-core's `overlay-l3`/`overlay-netstack` passthrough features exist ONLY for enrollment's WG-mint — thin clients must never enable them. Known residual: `remote_control` drags mongodb into every agent-side binary (5 `mongodb::` refs in audit/error/hub) — a `server` feature there is the follow-up.

**P3e Phase 2 (rc.368)** — three more levers on the rc.365 base:
- **Lever A — ffmpeg-next trimmed to `codec + format`.** `ffmpeg_next::init()` with default features calls `avdevice_register_all()` + filter registration, dragging avfilter (3.48 MiB) + avformat's demuxer table + avdevice + swscale into `roomlerd` — 23.07 MiB linked where the 10 HW encoders' true closure is 0.29 (measured with MSVC link stubs against the vendored static tree; −6.8 MiB from the trim). rc.71's breakage was codec-ALONE (`format` satisfies interrupt.rs/packet.rs); nothing in `src/encode/ffmpeg/` uses filter/device/scaling APIs (`format::Pixel` is the avutil enum). Verified live: `encoder-smoke --codec hevc` opened `hevc_nvenc` on an RTX 5090 and PASSED. Local feature-on checks need vcvars (bindgen wants MSVC's `stdint.h`) + `PKG_CONFIG_PATH` at a vendored FFmpeg tree.
- **`codegen-units = 1` as WORKFLOW env** (`CARGO_PROFILE_RELEASE_CODEGEN_UNITS: "1"` in release-agent.yml + release-tunnel.yml), deliberately NOT `[profile.release]` in Cargo.toml so the API docker build is untouched. Measured: roomlerd −18.4%, CLI −27.1% — and fat/thin LTO both LOSE to it on size AND build time for these binaries (thin is +5% size); don't "upgrade" to LTO without re-measuring.
- **The .deb ships the shim too** (`target/release/roomler-shim` installed as `usr/bin/roomler`, mirroring the MSI). The shim gained real Unix signal semantics: parent ignores SIGINT/SIGQUIT (foreground-group convention), child resets to SIG_DFL via `pre_exec`, kill-by-signal exits 128+n. This also structurally retires the 2026-08-06 CLI/daemon version-skew class — the command surface lives inside `roomlerd`, and a stale shim is a version-agnostic re-exec. Tunnel-only hosts keep the standalone binary from release-tunnel.yml.

**P3e Phase 3 + lever B (rc.371/374/377) — program complete.** Final field-verified state: Windows install 118 MB → 51.1 MiB on disk (`roomlerd.exe` 35, MSI **13.13 MB**), x86_64 `.deb` 64.2 → **10.44 MiB**, agent-host FFmpeg dir 150 → **5.7 MB**.
- **`remote_control` `server` feature** (default-ON): the Mongo audit DAO, the session `Hub`, and `Error::Mongo` are server-only; the five agent-side consumers set `default-features = false` — the mongodb driver is GONE from roomlerd/roomler/roomler-desktop/derp-relay graphs (tray 470 → 321 crates over the program). api/services/tests are unchanged.
- **Minimal vendored FFmpeg, BOTH platforms** (`vendor-ffmpeg-windows.yml`, jobs `build` + `build-linux-minimal`): `--disable-everything` + exactly the ten encoders `encode/ffmpeg/encoder.rs` dispatches (`*_nvenc`/`*_qsv`/`*_amf` × h264/hevc/av1 + `vp9_qsv`; the name lists are locked by unit tests). Windows = overlay port over the pinned vcpkg baseline (avcodec.lib 139.3 → 12.1 MB; ALWAYS re-bootstrap vcpkg.exe after the baseline reset); Linux = from-source n8.1.2 on ubuntu-22.04 (whole lib dir 2.5 MB). Configure gotchas are pinned in the workflow comments — the big three: `--disable-autodetect` suppresses the ffnvcodec/cuda auto-enable (the die message misleadingly blames "ffnvcodec"), qsv encoders need `--enable-parser=h264,hevc,av1` for their SEI/PS helper objects (else the shared lib carries an undefined `ff_hevc_decode_nal_sei`), and verification MUST be a runtime `find_encoder_by_name` probe (version scripts hide `ff_*` from `nm`; the build host's `/usr/local` hides missing DT_NEEDEDs — the staged-tree ldd assert closes that class).
- **`.deb` bundles only ldd-referenced libs** (fixpoint over DT_NEEDED): keep-set = `libavcodec libavutil libvpl libvpx` — `--as-needed` drops avformat and even swresample against the minimal tree, so the bundle guard floor is 2, and correctness is owned by the stock-24.04 load check.
- Runtime proof on real silicon: `hevc_nvenc` on two Blackwell-class GPUs, `hevc_qsv` on Iris Xe (pc50045), `mf-h264` cascade intact; `roomler … | head` no longer panics (SIGPIPE reset at `cli::run_from`). **amf remains CI-symbol-asserted only** (no AMD host in the fleet; the cascade falls through cleanly). **macOS note**: the vendored macOS FFmpeg is effectively dead weight — no `*_videotoolbox` names exist in the cascade, so Apple Silicon falls to SW; wiring VideoToolbox is a FEATURE decision, deliberately not made as part of the size program.

**Fleet RPC (remote command execution)** — run a command on a trusted device from `roomler exec` or the web device console, over the agent's **existing control WS** (deliberately NOT the overlay: the diagnostics this exists for are most needed when the mesh is broken). Wire = `rc:rpc.exec` / `rc:rpc.cancel` / `rc:rpc.result` / `rc:rpc.request` / `rc:rpc.response`; the hub parks the caller on a oneshot keyed by request id (`Hub::exec_on_agent`), the policy decision point is `crates/api/src/routes/agent_exec.rs::authorize`, execution is `agents/roomler-agent/src/exec.rs`. **Four independent default-deny gates**, each owned by a different party: org `TenantSettings.remote_exec_enabled` → caller's `permissions::EXEC_DEVICE` (1<<27, deliberately NOT in `DEFAULT_ADMIN`; `VIEW_EXEC_AUDIT` 1<<28 IS) → the device's `Agent.exec_policy` → the agent-local `exec_enabled` config key (the only refusal that survives a compromised server). ⚠️ Commands inherit the daemon's identity — **SYSTEM on Windows, root under systemd**. ⚠️ A caller AWAITS this frame, so unlike `Goodbye`/`UpdateNow` it must gate on `AgentCaps.rpc` containing `exec` (412 otherwise) — pushing to a pre-feature agent would hang the caller until its deadline. Every attempt including refusals lands in `exec_audit` (90 d TTL); output is redacted (agent token / `Bearer` / JWT-shaped) before it leaves the host. Diagnostic bundles (`roomler diag host|pair`) live in the CLI, not the agent, so a new probe is a CLI release rather than a fleet rollout. Full design in `docs/fleet-rpc.md`.

**Release trains & resumption after a session break** — the workspace version (`[workspace.package] version` in the root `Cargo.toml`) is a single `0.3.0-rc.N` bumped per merged PR; native artifacts are cut by tag-triggered workflows (`agent-v*` → `release-agent.yml` MSI/.deb/.pkg, `setup-v*` → `release-setup.yml`, plus `release-tunnel.yml`). **Always take the next tag from `git ls-remote --tags`, and re-fetch master before every merge / bump / tag.** There are no per-release handover files (retired 2026-05-23 in a privacy/security cleanup) — `git log` plus the `docs/` tree are authoritative.

**Older releases (0.1.x → 0.3.0-rc.26)**: CLAUDE.md no longer mirrors per-release notes — `git log` and `docs/remote-control.md` are authoritative for the historical arc. Key milestones, all shipped: live-verified WebRTC P2P (0.1.36), MF H.264 HW cascade with REMB-driven bitrate (0.1.26), codec negotiation H.264/HEVC/AV1 with probe-at-startup filter (0.1.28-0.1.30), clipboard + file-transfer data channels (0.1.32-0.1.33), WebCodecs canvas render bypass (0.1.36, Tier B7), agent lifecycle service hooks + auto-update (0.1.36), failure-resilience cycle with watchdog + crash rollback + SHA256 verification (0.1.50-0.1.54), heartbeat telemetry + pre-flight checks + opt-in Windows Service mode (0.1.55-0.1.58), M5 verification + clean-exit fixes + install-storm cooldown (0.1.61-0.1.63), M3 Z-path lock-screen overlay + browser auto-reconnect + perMachine MSI (0.2.0-0.2.5; A1 WGC NO-GO empirically confirmed), auto-update asset-picker flavour-aware (0.2.6), input regression fix (0.2.7), M3 A1 SystemContext-from-cold-start (rc.1-rc.7), UAC self-update + cross-flavour MSI cleanup + Tauri tray companion (rc.18), resumable file-DC transfers (rc.19-rc.20), ESET-evasive PROGRAMDATA staging (rc.21-rc.22), SystemContext Winlogon + elevated apps gating via `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP` (rc.26).

## Known Issues (OPEN only)

Fixed-and-shipped issues live in `git log`. Currently open:

- [LOW] [2026-08-03] Overlay ACL is feature-complete but **not yet field-proven under `enforce`**. `overlay_policies` + `OverlayNetwork.acl_mode` (`off`|`warn`|`enforce`, default `off`) shape the netmap per recipient, gate BOTH relay tiers (TURN relay-grant + `ws::derp_acl`'s precomputed per-network allow table), and compile **per-source ingress rules** onto `NetmapPeer.ingress_rules`. `overlay_rpf` (default `warn`) enforces all three node-side tiers: a peer can neither forge a SOURCE it doesn't own, nor address a DESTINATION outside the subnets this node advertises, nor reach a cidr/port/proto the tenant's policies didn't grant it. ⚠️ `ingress_rules` is `Option`: `None` = no ACL compiled (fall back to the coarse scope), `Some([])` = **deny** — never collapse them. Rules ship ONLY under `enforce`, so `warn` can never cause a node to drop. Remaining work is operational, not structural: nothing has run under `enforce` in the field, so flip a tenant to `warn` first and read `rx_denied` + the `overlay: inbound packet the sending peer is not entitled to send` lines before cutting over.
- [MEDIUM] [2026-08-05] OAuth account linking matches accounts by bare email (`find_or_create_oauth` step 2, `crates/services/src/dao/user.rs`). Microsoft's multi-tenant `common` endpoint accepts the attacker-settable `mail` attribute (nOAuth) → account takeover of any address via a hostile Entra tenant; fix = match Microsoft identities by `oid`+`tid` (or restrict to UPN), never `mail`. Google's `email_verified` gap was closed 2026-08-05 (stats PR-3); GitHub profile emails are verified-by-policy. The platform-admin allowlist deliberately uses user ObjectIds so this vector cannot reach platform-root.
- [MEDIUM] [2026-04-17] Remote-control: consent auto-granted on agent (no tray-driven prompt yet); fine for self-controlled hosts, needs UI for org-controlled devices per docs §11.2.
- [MEDIUM] [2026-05-23] File upload trusts the client-supplied `content-type` header (`crates/api/src/routes/file.rs:226-231`); no MIME whitelist. MIME-confusion risk when files are later served back.
- [LOW] [2026-05-23] Agent `config.toml` holds `agent_token`. Unix saves with `0600`; Windows currently relies on the default user ACL — `agents/roomler-agent/src/config.rs` should set an explicit ACL.
- [LOW] [2026-03-10] No git hooks configured (no pre-commit, no lint-staged).
- [LOW] [2026-04-20] Remote-control: NVIDIA NVENC `ActivateObject` returns 0x8000FFFF on RTX 5090 Blackwell for H.264 / HEVC / AV1 MFTs regardless of adapter binding. Cascade routes around it (H.264+HEVC land on alternative MFTs; AV1 has no alternative and is filtered from advertised caps by the probe-at-startup check). Worth re-testing on newer drivers / `CODECAPI_AVEncAdapterLUID` experiments.
- [MEDIUM] [2026-04-22] Browser viewer: Chrome's `<video>` enforces a ~80 ms jitter-buffer floor regardless of `jitterBufferTarget=0` / `playoutDelayHint=0`. Partial workaround shipped (opt-in WebCodecs canvas render path, Chrome-only) — flip on by default once field hours accumulate.

## Security Baseline

- JWT expiry: access=604800s (7 days), refresh=2592000s (30 days) (configurable via ROOMLER__JWT__*).
- Rate limiting: tower_governor 60 req/min per IP (2026-03-21).
- CORS (tightened 2026-07-28): unset `cors_origins` now allows ONLY the frontend's own origin (was: `Any`); explicit `"*"` keeps permissive mode with a startup warning; the restrictive branch enumerates methods/headers because `allow_credentials(true)` + wildcard is rejected by tower-http at request time (the old known-failing cors_tests pair).
- JWT default secret (2026-07-28): with `app.environment=production` (set in the prod configmap) the server REFUSES to boot on the built-in default secret; development keeps the loud warning.
- nginx security headers: X-Frame-Options, X-Content-Type-Options, Referrer-Policy, Permissions-Policy (2026-03-21); HSTS + CSP added 2026-07-28 (`files/nginx-pod.conf`). **CSP allowlist (corrected 2026-07-29, #252):** `script-src 'self' https://purestat.ai` (the site's own analytics, loaded in index.html); `connect-src 'self' wss: https: http://127.0.0.1:* http://localhost:*` — the loopback origins are REQUIRED: the remote-control viewer probes the local agent's loopback-TURN relay (`http://127.0.0.1:4798x/rc-local-turn`) and clipboard bridge (`rc-clipboard`, port bases 41989 + 47989). ⚠️ The initial CSP (#242) omitted both and broke analytics + the RC loopback-relay path in prod — the CSP validation had only exercised dashboard/auth/websocket, never the RC viewer. **When touching CSP, re-scan `ui/` for external + loopback endpoints (`grep -rhoE "https?://…"`) and exercise the remote-control page, not just the main SPA.**
- TURN `static-auth-secret` rotated out of the repo on 2026-05-23 — the committed `turnserver.conf` carries a `CHANGE-ME` placeholder; the live value lives in the operator's `ROOMLER__TURN__SHARED_SECRET` env.
- `Content-Disposition` filenames sanitized + RFC 5987 encoded on the file-download route (2026-05-23).
- Dependency-uplift pass (2026-07-29): **Rust** — cleared 4 advisories via precise semver-compatible lockfile bumps (crossbeam-epoch 0.9.18→0.9.20 RUSTSEC-2026-0204, memmap2 0.9.10→0.9.11 RUSTSEC-2024-0429, quinn-proto 0.11.14→0.11.16 RUSTSEC-2026-0185, spin 0.9.8→0.9.9 yanked). Deferred (need BREAKING direct-dep bumps + are not reachable in our usage): **rsa 0.7.2** Marvin timing ← web-push/jwt-simple — VAPID uses EC (ES256), not RSA-decrypt, so the timing oracle isn't reachable; **lopdf 0.26.0** stack-overflow ← genpdf (which is at its latest 0.2.0, upstream-blocked) — genpdf only GENERATES PDFs, never parses untrusted input, so the vulnerable parse path is unreached; **idna 0.5.0** punycode ← validator 0.18 (latest 0.21, 3 breaking minors); **quick-xml 0.38.4** ← transitive `^0.38` pin. Each remaining fix = its own breaking-bump PR with a full mars build+test cycle; tracked for a later pass. **JS** — the only runtime-reachable high is markdown-it→linkify-it (client-side ReDoS on crafted message links), but the fix is only in linkify-it 6.x which markdown-it 14 can't import (`default` export removed → build break), so it's ecosystem-blocked until markdown-it adopts 6.x. All other JS highs (jsdom→undici, @vue/test-utils→js-cookie/minimatch, vue-router→rollup) are dev/build tooling, never shipped in the prod bundle. Re-run: `bun audit` (ui) + `cargo audit` (mars, `~/.cargo/bin`).
