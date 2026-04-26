# Handover #10 — 0.1.36 → 0.1.41 + ArgoCD GitOps + VP9 4:4:4 plan

> Continuation of HANDOVER9 (which closed at 0.1.35 / Tier A complete +
> viewer controls). This window covered five interleaved threads:
>
> 1. **Operator quality polish** — REMB floor, HEVC→<video> fallback,
>    SW-codec demotion, even-dim guard, WebCodecs auto-fallback.
> 2. **Deploy infrastructure** — migrated from `scripts/build-and-push-image.sh`
>    to ArgoCD GitOps via `registry.roomler.ai`. Auto-sync 60 s.
> 3. **TURN enterprise bypass** — `turns:coturn.roomler.ai:443` over
>    both UDP and TCP, end-to-end through coturn iptables DNAT.
> 4. **CI hygiene** — Rust toolchain pinned to 1.95.0 across
>    `rust-toolchain.toml` and 5 workflow refs.
> 5. **Strategic plan + scaffolding for VP9 4:4:4** (the
>    "match mediasoup screenshare smoothness" goal) — encoder
>    backend + browser worker landed, transport + UI cutover
>    pending.

## Shipped commits (newest → oldest in this window)

```
794928c  feat(rc): VP9 4:4:4 decoder worker (Phase Y.2)
57c002e  feat(agent): VP9 4:4:4 encoder scaffolding (Phase Y.1) + plan
b34d4bd  fix(rc): auto-fallback to <video> when WebCodecs watchdog fires
78f12d7  feat(rc): demote SW-only HEVC/AV1 + log API 500 + harden room::list  [0.1.41]
64dbed3  fix(agent): raise REMB bitrate floor — 500 kbps unreadable           [0.1.40]
b46d27e  build: pin Rust toolchain to 1.95.0 in lockstep (repo + CI)
b3436f6  fix(agent): unbreak clippy 1.95 — sort_by_key + items_after_test_module
0afe5dd  style: cargo fmt on updater/peer/service — unbreak CI
2500c68  fix(rc): UI mounts <video> when WebCodecs falls back (HEVC / unsupported)
140697b  fix(turn): turns:host:443 both UDP+TCP, revert to coturn.roomler.ai
5f7272c  fix(rc): HEVC→<video> auto-fallback + plain :443 UDP/TCP TURN URLs
3e0d5f3  fix(rc): even-dim guard for rc:resolution — was killing HEVC encoder [0.1.39]
… (plus the agent-v0.1.38 / 0.1.37 reformat-and-bug-fix sequence)
```

Tags cut: `agent-v0.1.36` → `agent-v0.1.41` (six MSI/.deb/.pkg
releases in the period). All published as non-prerelease so the
auto-updater on 0.1.37+ pulls them in 6 h.

## What each major commit fixes

### 0.1.39 — even-dim guard

Field session 2026-04-24 sent `rc:resolution { mode:'fit',
width: 2154, height: 1077 }` (stage element height in CSS px ×
dpr=1). MF HEVC encoder requires even dims and bombed
`require non-zero, even dimensions, got 2154x1077`. HEVC has no SW
fallback on Windows so the cascade fail-closed to NoopEncoder —
zero-byte packets for the rest of the session, permanent black
frame on EVERY render path including WebCodecs (no frames to
decode). Fixed at both layers:

- Browser `applyFitResolution` floors to even via `& ~1`, clamps
  min 160×90.
- Agent `rc:resolution` handler does the same defensively.

### 0.1.40 — REMB bitrate floor

Field session showed `remb_bps=5000 → target_bps=500000`. At
1080p HEVC that's 2 % of target — chroma artefacts + unreadable
text. Replaced flat 500 kbps floor with `max(MIN_BITRATE_BPS,
base / 4)` (~2.5 Mbps at 1080p). `MIN_BITRATE_BPS` and
`MAX_BITRATE_BPS` promoted to module-level `pub const`.

### 0.1.41 — three fixes in one cut

1. **SW-only HEVC/AV1 demoted from advertised caps**. Caps probe
   now distinguishes `Hardware / SoftwareOnly / Failed` instead
   of just yes/no. `SoftwareOnly` codecs drop from the advertised
   list, forcing browser↔agent intersection to land on H.264
   where the cascade picks Intel QuickSync / NVENC / AMF. This
   is the architectural fix for "match mediasoup screenshare
   smoothness" on Intel iGPU hosts where the IHV HEVC MFT fails
   `ActivateObject 0x80004005` and we used to fall to SW HEVC
   `HEVCVideoExtensionEncoder`. Operator override:
   `ROOMLER_AGENT_ALLOW_SW_HEAVY=1`.
2. **`ApiError::Internal -> 500` logs the cause at ERROR level**
   so tower_http's "status=500" trace is no longer the only
   surface — actual reason now greppable in `kubectl logs`.
3. **`room::to_response` no longer panics on `r.id.unwrap()`** —
   defensive empty-string fallback. Believed to be the root
   cause of the 1ms-latency 500s in the field; logs since deploy
   show no recurrence.

### WebCodecs auto-fallback (b34d4bd)

Chrome's `RTCRtpScriptTransform` silently drops frames in current
builds for both HEVC and H.264 — `getStats` shows
framesReceived/framesDecoded climbing on the default decoder
while the worker's transform callback never runs. The 3-second
watchdog in the worker now triggers `stopWebCodecsPath()` →
`webcodecsActive = false` → view's `isWebCodecsRender` reverts →
Vue mounts `<video>` → existing srcObject watcher binds the
stream → user sees the video without reconnecting. Frames flow
through `pipeTo(transformer.writable)` to the default decoder
unchanged.

## ArgoCD GitOps deploy workflow

Migrated from `scripts/build-and-push-image.sh` (docker save +
scp + ctr import + kubectl rollout restart) to the new GitOps
flow documented in `CLAUDE.md` §Deployment. Critical knowledge
for future sessions:

- ArgoCD app `roomler-ai` tracks `master` of `roomler-ai-deploy`,
  path `k8s/overlays/prod`. Sync policy is now **Automated +
  self-heal**, controller `timeout.reconciliation: 60s`. A push to
  master takes ~60-90 s to roll out.
- Image built on mars: `docker build -t registry.roomler.ai/roomler-ai:vYYYYMMDD-HHMMSS-tag .`
  → pushed to self-hosted Docker Registry v2 → `sed -i` bumps
  `newTag:` in `k8s/overlays/prod/kustomization.yaml` → git push.
- `imagePullPolicy: IfNotPresent` so unchanged tags don't pull;
  the unique time-tag plus a `:latest` push handles both pinned
  and floating consumers.
- ArgoCD admin creds at `~/.argocd.auth` on mars
  (`ARGOCD_USERNAME=admin`). The `argocd login --grpc-web` token
  expires ~24 h; use `source ~/.argocd.auth && argocd login ...`
  when needed.
- Legacy `scripts/build-and-push-image.sh` still exists in the
  deploy repo but is **dead** — `imagePullPolicy: IfNotPresent`
  makes its kubectl rollout restart approach a no-op now.

## TURN :443 — five-URL build

`build_turn_config` (both copies — `state.rs` + `routes/remote_control.rs`)
expands the configmap base into:

```
turn:coturn.roomler.ai:3478                    (UDP)
turn:coturn.roomler.ai:3478?transport=tcp      (TCP)
turns:coturn.roomler.ai:5349?transport=tcp     (TLS/TCP standard)
turns:coturn.roomler.ai:443?transport=udp      (DTLS/UDP enterprise bypass)
turns:coturn.roomler.ai:443?transport=tcp      (TLS/TCP enterprise bypass)
```

Coturn server-side: parallel claude session on mars (project
`/home/gjovanov`) configured iptables DNAT :443 → worker:5349
(TCP + UDP) on all 3 K8s nodes (mars .74, zeus .226, jupiter .221)
with HMAC-SHA1 ephemeral creds. End-to-end TURN Allocate test
green on every IP × transport combination.

## VP9 4:4:4 plan — Phase Y

Strategic answer to "match mediasoup screenshare smoothness for
4:4:4 / crystal-clear text". Lives at `docs/vp9-444-plan.md`.

**Why Y exists**: Chrome's WebRTC video pipeline enforces 4:2:0
across every codec. RustDesk gets 4:4:4 by NOT using WebRTC.
We can match by routing encoded VP9 profile 1 (4:4:4 8-bit)
frames over an `RTCDataChannel` instead of a video track, and
decoding via WebCodecs `VideoDecoder({codec:'vp09.01.10.08'})`
which doesn't enforce the 4:2:0 constraint.

**Status**: Y.1 + Y.2 scaffolding shipped, Y.3 + Y.4 pending.

- Y.1 — `agents/roomler-agent/src/encode/libvpx.rs` with
  `Vp9Encoder` implementing `VideoEncoder` trait. BGRA→I444 via
  `dcv-color-primitives`, libvpx via `vpx-encode` 0.6 with
  screen-content tuning (cpu-used=8, lag-in-frames=0). Behind
  `vp9-444` Cargo feature. Default `full-hw` build is unaffected.
  Build prereq: system libvpx (`apt install libvpx-dev` /
  `vcpkg install libvpx:x64-windows-static-md`).
- Y.2 — `ui/src/workers/rc-vp9-444-worker.ts`. Independent of
  RTCRtpScriptTransform — fed via `RTCDataChannel.onmessage`
  forwarded by main thread. Frame assembler parses 13-byte
  length-prefixed header (`u32 size + u8 flags + u64 ts_us`),
  feeds VideoDecoder, paints OffscreenCanvas. 5 vitest cases
  lock the wire format.
- **Y.3 (pending)** — agent media_pump branch that routes encoded
  frames into a new `video-bytes` DC, plus back-channel control
  messages (`rc:vp9.request_keyframe`, `rc:vp9.bandwidth`,
  `rc:vp9.config`). Existing WebRTC video track stays added so
  SDP doesn't change but is unfed in this transport mode.
- **Y.4 (pending)** — caps probe extends to advertise
  `transports: ["webrtc-video", "data-channel-vp9-444"]`.
  Toolbar toggle "Crystal-clear (VP9 4:4:4)". Cutover flag
  defaults ON when both sides advertise.

## Known issues at 0.1.41

- **Chrome RTCRtpScriptTransform regression**: silently drops
  frames for HEVC + H.264 in current builds. Worked around with
  watchdog auto-fallback (b34d4bd). Long-term fix is Phase Y
  (DataChannel transport) or wait for Chrome to land a fix.
- **Intel HEVC `ActivateObject 0x80004005`**: persistent on
  Intel iGPU 165U + others; cascade falls to SW HEVC. Now
  demoted out of advertised caps (0.1.41) so H.264 wins
  negotiation.
- **NVENC Blackwell `ActivateObject 0x8000FFFF`**: persistent on
  RTX 5090 Laptop for H.264, HEVC, AV1. H.264 path falls through
  to MS SW H.264 MFT which delegates to async HW; HEVC + AV1
  fail outright (now demoted).
- **`/api/tenant/.../room` 500**: no recurrence in 24h since
  to_response defensive fix. Instrumentation in place if it
  comes back.

## Next session priorities

1. **Phase Y.3** — wire DataChannel video transport end-to-end.
   ~1-2 days, biggest remaining piece. Touches `peer.rs::media_pump`
   (large file, careful refactor). Adds `video-bytes` DC,
   `rc:vp9.*` back-channel, backpressure on `bufferedAmount`.
2. **Phase Y.4** — caps + UI cutover. Small, depends on Y.3.
   Toolbar toggle, A/B telemetry, default-on flag.
3. **CI install step for vp9-444 feature** — `release-agent.yml`
   needs `apt install libvpx-dev` (Linux) and
   `vcpkg install libvpx:x64-windows-static-md` (Windows agent
   runner) before flipping the feature on. Documented in the
   plan but not yet wired.
4. **Phase 0 tunings still applicable** (per planner critique
   in HANDOVER10 §VP9 plan): real `set_roi_hints` MF override
   and screen-content rate-control mode. Useful even if Phase Y
   ships — VP9 4:4:4 is opt-in / capable-host-only; the legacy
   path needs to stay competitive for low-end / Session-0 hosts.
5. **Backlog from HANDOVER9** still applies for Tier B11 (dirty-
   rect ROI), Tier B10 (TWCC BWE decoded locally), multi-monitor.

## Resumption recipe

Local rebuild (default features, no vp9-444):

```powershell
Get-Process roomler-agent -ErrorAction SilentlyContinue | Stop-Process -Force
cargo build -p roomler-agent --release --features full-hw
$env:RUST_LOG = "roomler_agent=info,webrtc=warn"
.\target\release\roomler-agent.exe run
```

Pre-commit (CI-equivalent) checks:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cd ui && bun run test:unit
```

Deploy UI/API to prod (ArgoCD GitOps):

```bash
ssh mars
cd /home/gjovanov/roomler-ai && git pull
TIME_TAG="v$(date +%Y%m%d-%H%M%S)-tag"
docker build -t registry.roomler.ai/roomler-ai:$TIME_TAG -t registry.roomler.ai/roomler-ai:latest .
docker push registry.roomler.ai/roomler-ai:$TIME_TAG && docker push registry.roomler.ai/roomler-ai:latest
cd /home/gjovanov/roomler-ai-deploy && git checkout master && git pull
sed -i "s|newTag:.*|newTag: $TIME_TAG|" k8s/overlays/prod/kustomization.yaml
git commit -am "chore(k8s): bump to $TIME_TAG (...)" && git push origin master
# ArgoCD auto-sync picks it up in <60 s — no manual sync needed
curl -sI https://roomler.ai/health
```

Cut an agent MSI release:

```bash
# Bump workspace.package.version in Cargo.toml, commit, then:
git tag agent-v0.1.42
git push origin master agent-v0.1.42
# release-agent.yml builds + publishes; updater on 0.1.37+ picks up in 6 h
```

## Plan file

`docs/vp9-444-plan.md` is the active plan. RustDesk-parity Tier A
(HANDOVER9) is fully shipped; Phase Y is the next strategic
investment. The legacy MF cascade is **not deprecated** and stays
the default for 18+ months minimum (planner critique was clear:
WebView2 / Wry / WKWebView don't actually match Chrome on
non-Windows platforms, so the native cascade IS the unattended
backend).
