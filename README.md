# Roomler AI

**Every device you own, one secure network.** Roomler puts the desktop of any of
your machines into a browser tab (TeamViewer / RustDesk class), joins all of your
devices into a private encrypted network that travels with you (Tailscale class —
with tunnels, SOCKS5, and exit nodes) — and throws in team chat and video
conferencing as part of every plan. One server, one identity, one small agent per
machine. Self-hostable, end-to-end encrypted.

<p align="center">
  <img src="docs/assets/hero-mesh.svg" alt="Your devices, one encrypted mesh — a laptop, a GPU workstation, a home server and a cloud cluster connected directly, coordinated by roomler.ai" width="640">
</p>
<p align="center"><i>Your devices, one encrypted mesh — coordinated from the cloud, connected directly.<br>The server never sees your screens, keystrokes, files, or traffic.</i></p>

## 🖥️ 1 · Desktop sharing & remote control

<p align="center">
  <img src="docs/assets/remote-desktop.svg" alt="A laptop anywhere showing the live desktop of an office PC inside a browser tab — the encrypted stream crosses the corporate firewall, mouse and keyboard flow back" width="760">
</p>

Open any of your machines in a browser tab and use it as if you were sitting in
front of it. Nothing to install on the viewing side — the machine you're reaching
runs a small agent, and your browser becomes its screen, keyboard, and mouse. The
picture is hardware-encoded and fluid enough for real work; clipboard, file
transfer, remote apps, and even the Windows lock screen just work. Every session
is consent-gated and audit-logged, end-to-end encrypted, and connects straight
through hotel Wi-Fi, NATs, and corporate firewalls.

| In detail | |
|---|---|
| **Viewer** | Plain browser tab — nothing to install on the viewing side. Classic `<video>` or low-latency WebCodecs canvas paths (bypasses Chrome's ~80 ms jitter buffer) |
| **Codecs** | H.264, HEVC, AV1, VP9 4:4:4 — hardware-encoded when the GPU allows (NVENC / Quick Sync / AMF / Media Foundation), with probe-and-rollback cascade and software fallback. See [`docs/encoders.md`](docs/encoders.md) |
| **Input & UX** | Full keyboard/mouse with keyboard-lock + Ctrl-Alt-Del, multi-monitor, resolution/scale control, remote cursor, clipboard sync (text/HTML/image), resumable file transfer, remote-apps menu (list / focus / launch, tmux re-attach), optional remote audio |
| **Unattended access** | Runs as a service, survives logout, controls the lock screen / UAC / pre-logon on Windows (SystemContext), virtual desktops for headless Linux servers |
| **Security** | Consent-gated + audit-logged sessions, tenant-scoped agent identity, E2E-encrypted WebRTC — the server relays only signalling |

## 🔐 2 · Your own secure private network

<p align="center">
  <img src="docs/assets/private-network.svg" alt="A laptop, a GPU box, a home server sharing its LAN and a cloud cluster joined in a private WireGuard mesh with stable names, crossing NAT and firewalls, with an exit-node option" width="760">
</p>

Every machine you enroll joins your own private network — as if all your devices
were on the same home LAN, wherever they actually are. Each one keeps a stable
address and a memorable name (`gpu-box`, `home`), traffic flows **directly
between your devices**, end-to-end encrypted with WireGuard, and nothing needs an
open port or a VPN concentrator. On top of that network: forward any port with
one command, point any app at a SOCKS5 doorway into a network only one of your
machines can see, share a whole home LAN through one device, SSH to any node by
name with no `sshd` exposed — or route all of your internet through a trusted
machine when you travel.

| In detail | |
|---|---|
| **WireGuard mesh** | Every node gets a stable private IP (CGNAT `100.64.0.0/10`) + a MagicDNS name. Carriers auto-negotiate: LAN-direct → public-direct → NAT hole-punch → TURN relay (QUIC-upgraded) → DERP over :443 — it works even from strict corporate networks. See [`docs/overlay-communication.md`](docs/overlay-communication.md) |
| **Tunnels** | `roomler forward` (any `host:port` an agent can reach becomes a local port), `roomler socks5` (per-agent or whole-fleet mesh proxy, TCP + UDP), declared routes supervised by the daemon |
| **SSH without sshd** | `roomler ssh <name>` — interactive shells on any node with no listening port and no `sshd` install, consent-gated ([`docs/roomler-ssh.md`](docs/roomler-ssh.md)) |
| **Exit nodes** | Route a client's entire internet egress (v4+v6, DNS included) through a chosen mesh peer — with a never-self-wedge safety model |
| **Multi-org** | One daemon can join several organizations at once, with per-org WireGuard keys and disjoint address blocks |
| **Policy & audit** | Default-deny tunnel ACLs, overlay ACLs (`off`/`warn`/`enforce`), subnet-route + exit-node approval in the admin UI, audit trail per flow |
| **Fleet operations** | `roomler exec` remote command execution (four independent default-deny gates), device console, live observability: mesh graph, per-carrier stats, relay usage |

## 💬 Bonus · Video conferencing & team collaboration

<p align="center">
  <img src="docs/assets/collaboration.svg" alt="A four-person video call with screen sharing next to a chat room with threads, reactions and typing indicators" width="760">
</p>

The team layer is built in, not bolted on: organized rooms with threaded chat,
reactions, mentions and file sharing, plus HD video calls with screen sharing and
recordings — on the same server and the same accounts as everything above. If
your team already lives here to reach its machines, meetings and chat come free.

<a href="roomler-intro.mp4">
  <img src="https://img.shields.io/badge/%E2%96%B6%20Watch-Collaboration%20Walkthrough%20(2%3A24)-009688?style=for-the-badge" alt="Watch the collaboration walkthrough video" />
</a>

| In detail | |
|---|---|
| **Rooms & chat** | Hierarchical room tree (text + voice/video), threaded messages, reactions, custom emoji, @mentions (TipTap editor), pins, embeds, typing indicators, presence |
| **Video conferencing** | [mediasoup](https://mediasoup.org/) WebRTC SFU, per-room calls, multiple producers/consumers, in-call chat, recordings |
| **Multi-tenancy** | Organizations with plans + Stripe billing, roles & permissions, invite links / email / batch invites, OAuth (Google, Facebook, GitHub, LinkedIn, Microsoft) |
| **Files** | Versioned + multipart uploads (S3/MinIO), cloud sync (Google Drive, OneDrive, Dropbox), AI document recognition (Claude vision) |
| **Search & export** | Global full-text search (Ctrl+K), XLSX + PDF conversation export, background task pipeline |
| **Notifications** | Real-time WebSocket + Web Push (VAPID), unread counts, mark-read flows |

## How it all fits together

For the technically curious — the control plane coordinates; your data takes its
own encrypted paths:

```mermaid
flowchart LR
    subgraph you["🧑 You"]
        B["Browser<br/>chat · calls · remote desktop<br/>· admin · observability"]
        T["roomler CLI<br/>forwards · SOCKS5 · exec"]
    end

    subgraph cloud["☁️ roomler.ai — coordination, not data"]
        S["API + signalling<br/>auth · enrollment · consent<br/>ACL policy · audit"]
        M["mediasoup SFU<br/>(conference media)"]
        R["TURN + DERP relays<br/>(fallback paths — ciphertext only)"]
    end

    subgraph fleet["🖥️ Your machines — each runs the roomlerd daemon"]
        A1["Office / home PC"]
        A2["Headless cloud server"]
        A3["GPU workstation"]
    end

    B -->|"sign in · policy · consent"| S
    T -->|"authenticate · ACL check"| S
    B <-->|"WebRTC (SFU):<br/>team video calls"| M
    A1 -.->|"always-on control link<br/>(outbound WSS)"| S
    A2 -.-> S
    A3 -.-> S

    B <==>|"WebRTC P2P, E2E-encrypted:<br/>screen · input · clipboard<br/>· files · apps · audio"| A1
    T <==>|"QUIC / WebRTC tunnels:<br/>TCP forward · SOCKS5"| A2
    A2 <-.->|"WireGuard overlay mesh:<br/>stable private IPs + MagicDNS,<br/>LAN-direct when possible"| A3

    B <-.->|"no direct path? relayed —<br/>still end-to-end encrypted"| R
    R <-.-> A1
```

Solid arrows are the **control plane** (login, policy, session setup). Thick double
arrows are **your data** — end-to-end encrypted, flowing directly between your device
and your machine whenever a direct path exists. The server introduces the two ends and
enforces policy; it never sees pixels, keystrokes, tunneled payloads, or overlay
plaintext. When no direct path can be punched, TURN/DERP relays forward ciphertext
they cannot read. Full picture: [`docs/architecture.md`](docs/architecture.md).

## Tech stack

| Layer | Technology |
|-------|-----------|
| **Backend** | Rust (edition 2024), Axum 0.8, Tokio, MongoDB 7, Redis (multi-pod fan-out) |
| **Remote desktop** | webrtc-rs (P2P, vendored forks for H.265 payloading + TURNS/TCP), OpenH264, Media Foundation, FFmpeg (NVENC/QSV/AMF), libvpx VP9 4:4:4, WebCodecs viewer |
| **Overlay / tunnels** | boringtun (WireGuard), quinn (QUIC), smoltcp userspace netstack, Wintun / tun / utun, coturn + standalone DERP relays |
| **Conference media** | mediasoup 0.20 SFU + mediasoup-client |
| **Native apps** | `roomlerd` daemon (Rust), `roomler` CLI, `roomler-desktop` tray companion (Tauri 2), `roomler-setup` install wizard (Tauri 2) |
| **Frontend** | Vue 3, Vuetify 3, Pinia, TipTap v3, d3 (observability) |
| **Auth** | JWT (6 audience-checked token types), Argon2, OAuth 2.0 |
| **Infrastructure** | Docker multi-stage image (nginx + API), docker-compose dev stack (MongoDB, Redis, MinIO, coturn), Kubernetes-ready |

## Quick start

Using the hosted service? Create a workspace at [roomler.ai](https://roomler.ai)
and skip straight to *Add a machine*.

### Run the platform (development)

```bash
# 1. Infrastructure
docker compose up -d          # MongoDB :27019, Redis :6379, MinIO :9000, coturn

# 2. Backend
cargo run --bin roomler-ai-api    # API on :3000

# 3. Frontend
cd ui && bun install && bun run dev   # SPA on :5000
```

### Add a machine to your fleet

Mint an enrollment token in the admin UI (**Admin → Agents → Enroll**), then on the
machine:

```bash
# Linux / macOS — daemon (remote desktop + tunnels + overlay)
curl -fsSL https://roomler.ai/api/setup/install.sh | sh -s -- --role daemon --token <enrollment-jwt>

# Windows (PowerShell)
irm https://roomler.ai/api/setup/install.ps1 | iex
```

Prefer a GUI? Download the **roomler-setup** wizard from the admin UI — one installer
for every role (Windows service / per-user / tunnel-only) on every OS. Details, MSI
flavours, and service modes: [`docs/installation.md`](docs/installation.md).

### Reach it

- **Remote desktop**: open the machine from the web app and click *Connect*.
- **Tunnel**: `roomler forward --agent <name> --local 127.0.0.1:5432 --remote db:5432`
- **Overlay**: `roomler ping <name>` — every node has a stable IP and MagicDNS name.

## Platform support

| Component | Windows | Linux | macOS |
|---|---|---|---|
| **`roomlerd` daemon** (desktop + tunnel exit + overlay node) | x64 — MSI (per-user / per-machine), signed | x86_64 + arm64 — `.deb` / tarball, systemd | arm64 — `.pkg`, LaunchAgent |
| **`roomler` CLI** (tunnel client) | x64 zip | x86_64 tarball + `.deb` | universal tarball |
| **`roomler-setup` wizard** | x64 (signed) | x86_64 | universal |
| **`roomler-desktop` companion** (tray, consent, tunnels UI) | ✅ | — | — |
| **Viewer / web app** | any modern browser; WebCodecs low-latency paths are Chrome-first | | |

All release artifacts ship with SHA-256 sidecars, GPG signatures, and SLSA build
provenance (`gh attestation verify --repo gjovanov/roomler-ai`).

## Documentation

**[→ Full documentation index](docs/README.md)** · new here and not an engineer?
Start with [Use cases](docs/use-cases.md).

| Start with | |
|---|---|
| [Use cases](docs/use-cases.md) | What people do with it — in plain language, scenario by scenario |
| [Agent & Tunnel overview](docs/agent-tunnel-architecture.md) | The remote-access stack in five minutes, for users and operators |
| [Installation](docs/installation.md) | Every install path on every platform, enrollment, self-update |
| [Remote control](docs/remote-control.md) | Full remote-desktop design: protocol, security, latency budget |
| [Encoders](docs/encoders.md) | Codec × platform matrix, hardware cascade, rate control |
| [Rate control](docs/rate-control.md) | The Priority dial, per-session rate loops, and why resolution never flips mid-motion |
| [Overlay communication](docs/overlay-communication.md) | Every carrier path, inside and outside a corporate VPN |
| [Tunnels](docs/tunnels.md) | Forwards, SOCKS5, declared routes, QUIC-over-TURN |
| [Roomler SSH](docs/roomler-ssh.md) | SSH into any node — no `sshd`, no bound port |
| [Architecture](docs/architecture.md) | The whole system: control plane vs three data planes |
| [API reference](docs/api.md) | Every HTTP route + the WebSocket surfaces |

## License

MIT
