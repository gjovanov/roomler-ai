---
paths:
  - ".github/workflows/promote.yml"
  - ".github/workflows/hosted-image.yml"
  - "Dockerfile*"
  - "docker-compose*.yml"
  - "docs/deployment.md"
  - "docs/multi-pod-scale-out.md"
  - "crates/modules/conference/**"
---

# Deployment — production topology and the media-path traps

<!-- Moved verbatim from CLAUDE.md on 2026-10-28, when that file had grown to 58 KB and was
     loaded on every prompt and into every subagent. CLAUDE.md keeps a compressed version; release and promote mechanics are the `ship-it` skill. This file loads only when Claude reads
     or edits a file matching `paths:` (Claude Code path-scoped rules). Keep it under 40,000
     characters: from Claude Code 2.1.309 an on-demand file is cut there. Block HTML comments like
     this one are stripped before the model sees the file. -->

## Deployment

**Production**: `https://roomler.ai/` — the `--server` for enrollments and the
origin the browser controller loads. Topology, k8s layout, tenant-affinity LB and
the media-path rules: **`docs/deployment.md`** + `docs/multi-pod-scale-out.md`.
Release, signing and promotion mechanics: the **`ship-it` skill**.

- **Pipeline (FR-73)**: Actions builds
  `ghcr.io/gjovanov/roomler-ai:hosted-<YYYYMMDD>-<sha7>` on every merge to master
  that can change the image; **`gh workflow run promote.yml -f tag=…`** bumps the
  deploy repo and ArgoCD (Automated + selfHeal + prune, webhook) rolls it within
  ~5 s. ⚠️ **Build ≠ deploy — nothing rolls until someone promotes.** ⚠️ The lane
  never writes `latest`; that is the self-host `full` image (no `saas`).
- **Pods**: namespace `roomler-ai`, deployment **`roomler2`** (not `roomler-ai`), 2
  replicas forced apart by `podAntiAffinity`, hostNetwork, RollingUpdate
  maxSurge 0 / maxUnavailable 1 (surge 0 because hostNetwork can't double-bind).
  Scheduling is pinned to `tier=high-performance` nodes; the Mongo/MinIO
  StatefulSets keep their hostname pins (node-local storage).
- ⚠️ **Every mediasoup-serving cluster host needs the RTC-range (40000–49999)
  DNAT/SNAT/TTL rules** in its `COTURN_*` iptables chains. One node missing them
  gave flawless signalling and **zero media, for weeks, with nothing logged**
  (`connect_transport` only records the client's DTLS params — it proves nothing
  about packets). Because of the tenant-affinity hash this breaks a *consistent
  subset of tenants*: **"video works for org A but not org B" is a per-NODE
  media-path suspect, not an app bug.** Provisioning lives in the Ansible host vars
  and the playbook **flushes and rebuilds** those chains — never hand-fix a host
  without also fixing the vars. App-side tell: `media transport has no DTLS 15s
  after connect_transport` in the pod log.
  ⚠️ The same DNAT swallows the **host's own mesh node**. The overlay's default
  port band (43648–44415) sits inside the range. So a serving host's `roomlerd` pins
  `overlay_direct_port = 21640`, backed by a HOST_FW allow and
  `ip_local_reserved_ports=40000-49999`. Without the pin, a peer's first packet lands
  in the VM and the host rides DERP after every roll (#1665, `docs/deployment.md`).
- ⚠️ **Tenant affinity pins long-lived sockets.** The front proxy hashes on the
  tenant key so a tenant's users, agents, tunnel clients, DERP sockets and
  mediasoup rooms land on ONE pod (the rc-hub / tunnel-hub / DERP relay / room
  registry are pod-local). After any upstream flip, **also
  `kubectl rollout restart deploy/roomler2`** so long-lived WSs re-hash.
  ⚠️ A listing's `is_online` is heartbeat-based (HTTP) while rc/tunnel need the
  pod-local hub — an agent whose control WS is half-open (a TLS-inspecting corp
  middlebox keeps ACKing keepalives after a pod roll killed the upstream leg) shows
  GREEN but is `agent_offline`; agents ≥rc.293 self-heal in ≤~2 min via a
  receive-liveness deadline.
- **After every roll, field-verify from the fleet** — pods on the new image,
  online-agent count, an RC session, an overlay pair, a tunnel forward. The
  workflow only proves the public `/health` kept answering. ⚠️ An exec sweep is a
  **biased sample** (it reaches only what is online) — query the server for the
  denominator.
