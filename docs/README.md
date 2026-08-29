# Roomler AI — documentation index

Roomler is three products on one platform, in this order: **1 · desktop sharing &
remote control**, **2 · your own secure private network** (WireGuard overlay,
tunnels, SOCKS5), and — as the included bonus — **video conferencing & team
collaboration**. The map below shows how the docs hang together; the tables list
every document with its audience.

> **Not an engineer?** Start with [use-cases.md](use-cases.md) — plain-language,
> scenario by scenario, with pictures. The main [README](../README.md) has the
> product tour.

```mermaid
flowchart TB
    subgraph entry["Start here"]
        UC["use-cases.md"]
        ATA["agent-tunnel-architecture.md"]
        ARCH["architecture.md"]
    end

    subgraph rd["🖥️ 1 · Remote desktop"]
        RC["remote-control.md"]
        ENC["encoders.md"]
    end

    subgraph net["🔐 2 · Private network & tunnels"]
        OC["overlay-communication.md"]
        TUN["tunnels.md"]
        MO["multi-org.md"]
    end

    subgraph collab["💬 Bonus · Collaboration"]
        RT["real-time.md"]
        UIX["ui.md"]
    end

    subgraph ops["🔧 Install & operate"]
        INST["installation.md"]
        DEP["deployment.md"]
    end

    REF["api.md · data-model.md · testing.md"]

    UC --> rd & net & collab
    ATA --> rd & net
    ARCH --> rd & net & collab
    rd & net --> ops
    rd & net & collab --> REF
```

## Start here

| Doc | What it covers |
|---|---|
| [use-cases.md](use-cases.md) | Scenario walkthroughs across all three pillars in plain language, plus the permission model |
| [self-hosting.md](self-hosting.md) | Running the whole product yourself — one compose file, TLS termination, the conference media-port caveat, upgrades and backups |
| [compare/](compare/README.md) | Head-to-head against Tailscale, RustDesk, TeamViewer, MeshCentral and NetBird — each page naming what the other product does better, first |
| [agent-tunnel-architecture.md](agent-tunnel-architecture.md) | The remote-access stack (daemon + CLI + coordination) in five minutes — written for end users and operators |
| [architecture.md](architecture.md) | The whole system: control plane vs the three data planes, workspace crate map, deployment topology |

## 🖥️ 1 · Desktop sharing & remote control

*Any of your machines, live in a browser tab — nothing to install on the viewing
side, consent-gated, end-to-end encrypted.*

| Doc | What it covers |
|---|---|
| [remote-control.md](remote-control.md) | Full design: topology, agent internals, `rc:*` signalling, consent/security model, latency budget |
| [encoders.md](encoders.md) | Codec × platform × backend matrix, the hardware-encoder cascade, rate control, capture backends, viewer decode paths |
| [rate-control.md](rate-control.md) | How a session spends its bits: the Priority dial, the per-session control loops, why resolution never flips mid-motion (rc.445), crisp-at-rest, config reference |

## 🔐 2 · Your own secure private network

*All your devices on one private, encrypted network with stable names — plus
port forwards, SOCKS5, SSH without sshd, and exit nodes on top.*

| Doc | What it covers |
|---|---|
| [overlay-communication.md](overlay-communication.md) | **Start here for the overlay** — every carrier path (LAN, public, hole-punch, relay, DERP), inside and outside a corporate VPN, with field-proof |
| [overlay-nat-traversal.md](overlay-nat-traversal.md) | The carrier cascade mechanics: NAT-type probing, srflx hole-punch, cooldowns, PathMonitor |
| [overlay-exit-nodes.md](overlay-exit-nodes.md) | Tailscale-style exit nodes: full-egress routing (v4+v6+DNS) with the never-self-wedge safety model |
| [overlay-wfp.md](overlay-wfp.md) | Windows: surviving a Group-Policy-locked firewall via the Windows Filtering Platform |
| [multi-org.md](multi-org.md) | One device in N organizations: `[[orgs]]`, address blocks, the shared carrier plane, mux NAT |
| [tunnels.md](tunnels.md) | Concepts & protocol: forwards, SOCKS5 (TCP+UDP), mesh mode, declared routes, transports, LocalAPI, CLI |
| [tunnel-install.md](tunnel-install.md) | Step-by-step runbook: install, enroll, ACL policy, open and test a forward from a corporate network |
| [fleet-rpc.md](fleet-rpc.md) | `roomler exec` remote command execution: transport, the four default-deny gates, audit |
| [device-naming.md](device-naming.md) | Fleet name vs MagicDNS label, admin rename + overlay propagation, display_name/tags, the rehydrate-clobber rule, rename-proof exit-node pinning |
| [roomler-ssh.md](roomler-ssh.md) | SSH into any node by overlay address with no `sshd` and no bound port — why the packets are intercepted below the OS, the four default-deny gates, interactive shells on Unix and Windows, `sftp`/`scp`, port forwarding, and the audit + activity records |
| [remote-config.md](remote-config.md) | **PLAN** — enabling exec / SSH from the dashboard without making gate 4 server-settable: the device-local opt-in, why server-derived state was rejected, primary-only under multi-org, and who may flip a switch they cannot themselves walk through |

## 💬 Bonus · Video conferencing & team collaboration

*Rooms, threaded chat, and HD calls — included with the platform, running on the
same accounts and server.*

| Doc | What it covers |
|---|---|
| [real-time.md](real-time.md) | The WebSocket surfaces: user events, presence, mediasoup signalling, the `rc:*` agent protocol, DERP |
| [ui.md](ui.md) | Frontend map: views, stores, composables, the remote-desktop viewer, observability components |

## 🔧 Install & operate

| Doc | What it covers |
|---|---|
| [installation.md](installation.md) | Every install path: wizard, MSI flavours, `.deb`/`.pkg`, terminal installers, enrollment, service modes, self-update |
| [code-signing.md](code-signing.md) | How every published artifact is signed: Azure Artifact Signing over GitHub OIDC, macOS notarisation, GPG + build provenance, and the operator scripts that (re)establish the credentials |
| [linux-self-update.md](linux-self-update.md) | Design of the Linux self-update path (tarball as the universal artifact) |
| [deployment.md](deployment.md) | Deploying the server: Docker image, dev compose stack, environment, health, release pipelines |
| [multi-pod-scale-out.md](multi-pod-scale-out.md) | The settled multi-pod architecture: identity, tenant-affinity routing, mediasoup scale ladder |
| [operator-systemcontext-smoke.md](operator-systemcontext-smoke.md) | Operator checklist: verifying Windows SystemContext (pre-logon control) on a field host |
| [testing.md](testing.md) | Test suites and harnesses: integration, unit, E2E, capture smoke, k8s E2E lane |
| [api.md](api.md) | Every HTTP route (method + path + purpose) and the auth model |
| [data-model.md](data-model.md) | Every MongoDB collection with ER diagrams, indexes, TTLs |

## 📐 Design records

Point-in-time design documents for features that are in flight or deliberately
deferred. They record *why*, not current behaviour — the feature docs above stay
authoritative.

| Doc | Status |
|---|---|
| [overlay-session-proof.md](overlay-session-proof.md) | In flight — moving the network plane out of the Windows session (`netd`, flag-off scaffold) |
| [overlay-warm-relay.md](overlay-warm-relay.md) | Shipping — a UDP relay leg that survives the corporate VPN (C4) |
| [overlay-symmetric-punch.md](overlay-symmetric-punch.md) | Design — symmetric-NAT-aware punch completion via observed-source promotion |
| [moq-remote-desktop-evaluation.md](moq-remote-desktop-evaluation.md) | Deferred — Media-over-QUIC evaluated for the remote desktop; revisit criteria inside |
