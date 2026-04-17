# Remote Control — Handover #2

> Second handover note on the remote-control subsystem. The first was
> `HANDOFF.md` — written after the server-side scaffolding was scoped
> but not wired up. This one picks up *after* a full implementation
> sprint and is for a fresh Claude Code session picking the work back
> up on the deployment server (where a real display + X11 dev libs
> are available).
>
> **Read order when resuming**: this file → `docs/remote-control.md`
> → the commit chain listed below → start wherever the "What's next"
> section says.

## tl;dr

All three layers of the subsystem are written and pushed to `master`
(10 commits, listed below). The build is green under
`--features full` on a Linux host with X11 dev packages installed.
Integration tests exercise the HTTP enrollment, WebSocket signalling,
and a real WebRTC SDP/ICE round-trip using `webrtc-rs` on both sides.

What is **not** verified yet: a live agent against a live browser on
a real display, end to end. That's the first thing to do on the
deployment server.

---

## Commit chain (this session)

Every commit leaves `cargo check --workspace` and the existing 114-
test suite green. Order matters — the later ones build on earlier
models and wire.

```
68d5e57  chore: add scripts/dev-xvfb.sh for headless capture smoke tests
9c11db3  feat(remote-control): input injection via enigo — end-to-end control
429160b  feat(agent):         OpenH264 encoder + live WebRTC video track
5036470  feat(agent):         screen capture backend via scrap (XShm/DXGI/CG)
b6ffe44  feat(remote-control): browser controller view + WS rc:* channel
61c2913  feat(agent):         real webrtc-rs PeerConnection (SDP + ICE)
fee3e93  refactor(remote-control): pin ObjectId wire format to raw hex strings
05321e0  feat(agent):         native remote-control agent skeleton (signaling-only)
980894b  feat(remote-control): admin Agents panel
43f0c5b  feat(remote-control): TeamViewer-style remote desktop foundation (rename + crate + REST + WS + tests)
```

Start reading at `43f0c5b` if you need to reconstruct the design; each
commit message is intentionally detailed and explains the **why**, not
just the diff.

---

## Where we are end-to-end

```
┌──────────────────────────────────────────────────────────────┐
│ roomler-agent --features full                                │
│   enroll ✓  →  connect ✓  →  screen capture (scrap) ✓       │
│   H.264 encode (openh264) ✓  →  RTCPeerConnection track ✓   │
│   incoming input DC → enigo injection ✓                     │
└──────────────────────────────────────────────────────────────┘
                             ↕ rc:*
┌──────────────────────────────────────────────────────────────┐
│ Roomler API server                                           │
│   Hub · signalling relay · audit · REST · TURN creds         │
└──────────────────────────────────────────────────────────────┘
                             ↕ rc:*
┌──────────────────────────────────────────────────────────────┐
│ Browser (admin → Agents → Connect)                           │
│   RTCPeerConnection · video element · input capture          │
│   pointer/wheel/key → input DC → agent's OS ✓                │
└──────────────────────────────────────────────────────────────┘
```

### What compiles and is covered by tests

| Layer | Tests | Status |
|---|---:|:---:|
| `roomler-ai-remote-control` unit (session state, consent, TURN creds, signalling, wire-format locks) | 20 | ✅ |
| `roomler-ai-services::auth` unit (agent + enrollment JWTs, audience separation) | 5 | ✅ |
| `roomler-agent` unit (default features) | 5 | ✅ |
| `roomler-agent` unit (`--features openh264-encoder`) | +4 | ✅ |
| `roomler-agent` unit (`--features enigo-input`) | +3 | ✅ |
| `roomler-agent` unit (`--features scrap-capture`) | +1 | ✅ (skips without a display) |
| UI stores + composables (including `ws`/`agents`/`useRemoteControl`) | 259 | ✅ |
| Integration: remote-control REST + WS | 10 | ✅ |
| Integration: agent library against a TestApp (real WebRTC round-trip) | 4 | ✅ |

### What is still TODO before a real demo

1. **Run the agent on a real Linux desktop, point it at a live API, open
   the browser viewer.** No one has done this yet — only the unit +
   integration paths above are verified. Expect to find something that
   broke in the seam between verified layers.
2. **Clipboard plane** — data channel accepted on both sides, handlers
   still log-only. Needs `arboard`-style wiring on the agent, and a
   browser-side hook to `navigator.clipboard`.
3. **File transfer plane** — same story; chunked file send/receive over
   the `files` data channel is unimplemented.
4. **TWCC / adaptive bitrate** — encoder runs at a fixed 3 Mbps.
   `Openh264Encoder::set_bitrate` is a no-op.
5. **Consent UI on the agent** — auto-grants today. docs §11.2 wants a
   tray prompt for org-controlled devices.
6. **Multi-monitor capture** — scrap enumerates displays but the
   pump only uses `Display::primary()`.

---

## Running it on the deployment server

The deployment server is per `CLAUDE.md` a real Linux host with X11
installed, so most of the WSL hand-wringing this session went through
disappears. You should be able to just:

```bash
# 1. Clone + system deps (one-time)
sudo apt update
sudo apt install -y \
    libxcb1-dev libxcb-shm0-dev libxcb-randr0-dev \
    cmake libclang-dev pkg-config   # rust / openh264 / webrtc-rs build deps

# 2. Build the agent binary
cargo build -p roomler-agent --release --features full

# 3. Bring up MongoDB + Redis + COTURN + MinIO for the API
docker compose up -d

# 4. Run the API in one terminal
cargo run --bin roomler-ai-api
#   → listens on http://localhost:3000 by default

# 5. Run the Vite dev server in another terminal (optional, for the
#    browser controller UI during dev)
cd ui && bun install && bun run dev
#   → http://localhost:5000 proxies /api and /ws to :5001 (prod) or
#     directly to localhost:5001 in dev

# 6. Enrol the agent (in a third terminal)
#    - register/log in to the UI as an admin
#    - go to /tenant/<tenantId>/admin → Agents → "Issue enrollment token"
#    - copy the one-shot JWT and run:
./target/release/roomler-agent enroll \
    --server http://localhost:3000 \
    --token <paste-the-enrollment-jwt> \
    --name "$(hostname) desktop"

# 7. Start the agent
./target/release/roomler-agent run
#   → you should see "rc:agent.hello sent" and the agent row in the
#     admin UI turn green / is_online=true.

# 8. In the browser: /tenant/<tenantId>/agent/<agentId>/remote
#    Click Connect. Expected phases: requesting → awaiting_consent
#    → negotiating → connected. Video should start flowing; typing +
#    clicking on the video surface should drive the remote desktop.
```

### Things likely to bite on first try

- **COTURN secret**: TURN only works if `settings.turn.shared_secret`
  is set (see `crates/config/src/settings.rs` → `TurnSettings`). Without
  it the `/api/turn/credentials` endpoint returns STUN only, which is
  fine for localhost but won't traverse real NAT.
- **JWT secret**: `ROOMLER__JWT__SECRET` defaults to
  `"change-me-in-production"` — change it before any real deployment.
  Agents enrolled under one secret can't authenticate to a server
  running under another (their token signature no longer verifies).
- **`DISPLAY` on a server host**: if the deployment server is literally
  a server (no GUI session), the agent has nothing to capture. Either
  run it on a desktop machine, or use `scripts/dev-xvfb.sh` to do a
  smoke test inside an Xvfb (but capture will show the Xvfb contents,
  not the desktop).
- **Feature flags**: default build has no capture, no encode, no input.
  That's intentional (CI friendliness) but means a default build of the
  agent is useless in production. Always build with `--features full`
  on the real agent host.
- **WebRTC over HTTP**: the browser's `RTCPeerConnection` works over
  plain `http://localhost` in dev, but most browsers refuse to expose
  `getDisplayMedia` + microphone over non-TLS for non-localhost
  origins. For the viewer the PC itself is fine over HTTP.

---

## Quick test commands

Useful for a sanity sweep after pulling the branch on a new host.

```bash
# Workspace compiles
cargo check --workspace

# Lib + unit tests (no MongoDB required)
cargo test -p roomler-ai-remote-control --lib
cargo test -p roomler-ai-services --lib auth::
cargo test -p roomler-agent --lib

# Full feature-gated agent tests (needs X11 dev libs installed)
cargo test -p roomler-agent --lib --features full

# Headless capture smoke test via Xvfb (needs xvfb + xterm installed)
./scripts/dev-xvfb.sh

# Integration tests (needs MongoDB on :27019 — use `docker compose up -d mongo`)
cargo test -p roomler-ai-tests --lib remote_control_tests::
cargo test -p roomler-ai-tests --lib agent_tests::

# UI unit tests (needs `cd ui && bun install` once)
cd ui && bun run test:unit

# UI type + build check
cd ui && bun run build
```

---

## Design contracts that must not break

Same as HANDOFF.md #1 but updated for where we are now:

- **The server never sees raw input or pixels.** They travel on the
  direct P2P (or TURN-relayed) PeerConnection between agent and
  browser. Any instinct to "proxy this one thing through the server"
  is the wrong move; route over the data channels or a new DC instead.
- **One `ObjectId` per `RemoteSession`** on the wire. Serialised as a
  raw 24-char hex string (lock-in tests live in
  `signaling::tests::object_ids_serialise_as_raw_hex_on_wire` and
  `serde_helpers::tests`). Do not regress to bson extended JSON.
- **Permissions on the wire are pipe-separated strings**
  (`"VIEW | INPUT"`), not numeric. bitflags 2.x ignores
  `#[serde(transparent)]`; do not try again.
- **`Hub::dispatch` is the single entry point** from the WS layer.
  Everything goes through the state machine; no ad-hoc bypasses.
- **TURN credentials are short-lived (10 min).** The shared secret
  lives in settings, *never* in a client, *never* in a response.
- **Feature flags gate any transitive system dep.** `scrap-capture`
  needs libxcb\*-dev; `openh264-encoder` compiles C from source;
  `enigo-input` needs nothing extra. The default build has to succeed
  in a bare `rust:bookworm` image.

---

## Files worth knowing about

Grouped by the layer you'd touch for a given change.

| If you want to … | Start at |
|---|---|
| Add a new `rc:*` message | `crates/remote_control/src/signaling.rs` |
| Change the session state machine | `crates/remote_control/src/{hub,session,consent}.rs` |
| Add / change a REST endpoint | `crates/api/src/routes/remote_control.rs` |
| Change what agent tokens look like | `crates/services/src/auth/mod.rs` |
| Tweak the WS routing / agent handshake | `crates/api/src/ws/{handler,remote_control}.rs` |
| Change the agent's capture pipeline | `agents/roomler-agent/src/capture/` |
| Change encoder selection / bitrate control | `agents/roomler-agent/src/encode/openh264_backend.rs` |
| Change what OS events get injected | `agents/roomler-agent/src/input/enigo_backend.rs` |
| Change the WebRTC PC (tracks, DCs, codec) | `agents/roomler-agent/src/peer.rs` |
| Change the browser viewer UX | `ui/src/views/remote/RemoteControl.vue` |
| Change how the browser emits input events | `ui/src/composables/useRemoteControl.ts` |
| Add an admin action on agents | `ui/src/components/admin/AgentsSection.vue` + `ui/src/stores/agents.ts` |

---

## What's next — priority ordered

1. **Smoke-run the full stack on the deployment server.** Clone,
   `--features full` build, enrol, connect from a browser. Expect
   something to bite. Fix, commit.
2. **Clipboard plane.** Agent: wire `arboard` to the `clipboard` DC.
   Browser: `navigator.clipboard.readText/writeText` bound to the same
   DC. Direction bits in the audit log per docs §11.
3. **Consent prompt on the agent.** A small Tauri / eframe window,
   not a CLI flag. docs §11.2 has the design.
4. **File transfer plane.** Chunked, resumable. docs §5.2 sketches
   the framing.
5. **Adaptive bitrate.** Hook up TWCC feedback from webrtc-rs, feed
   into `Openh264Encoder::set_bitrate` (needs crate-version-specific
   plumbing we punted on — see the TODO in `openh264_backend.rs`).
6. **Multi-monitor capture.** `scrap::Display::all()` gives the list;
   plumb `mon` through `Frame`, `InputMsg`, and the browser so
   coordinates resolve correctly across monitors.
7. **Install signing / packaging.** MSI for Windows, notarised .pkg
   for macOS, .deb + systemd-user unit for Linux. None of this exists
   yet.

---

## Deferred / punted

- **Hardware encoders** (NVENC, QSV, VAAPI, VideoToolbox, Media
  Foundation). Traits allow for them; no backends written. Each is
  a feature flag + a concrete `VideoEncoder` impl.
- **Wayland capture via PipeWire portal.** scrap is X11-only on
  Linux. Wayland is doable through `pipewire-rs` + the
  `xdg-desktop-portal` ScreenCast interface — a future backend.
- **Mobile controller**. docs flagged this; touch input is stubbed
  in `InputMsg::Touch` but enigo has no cross-platform touch API
  so it currently drops silently.
- **SFU bridge** for multi-watcher sessions. docs §4.3 has the shape.
  Current code is 1:1 (P2P) only.
- **Recording**. The `recording_url` field exists on `RemoteSession`;
  nothing writes to it yet.

---

## Gotchas the commit messages bury

- `openh264 = { default-features = false, features = ["source"] }`
  compiles OpenH264 from C on first build. Adds ~30–60 s to a clean
  build on a fast box; much longer on a Pi. The binary artefact is
  cached per target so only hurts once per target triple.
- `webrtc-rs` 0.12 is pinned. 0.20.0-alpha.1 is the current head;
  do not upgrade casually — the API surface for `PeerConnection`,
  `TrackLocalStaticSample`, and `MediaEngine` shifts between releases.
- `scrap` pins the `Capturer` to its creating thread (the XShm
  handles have thread affinity on Linux). Same story for `enigo` and
  the openh264 `Encoder`. All three backends use a dedicated OS
  thread with an mpsc command channel; do not "optimise" them into
  `tokio::task::spawn_blocking` — that'll crash non-deterministically.
- `bitflags 2.x` ignores `#[serde(transparent)]`. Lock-in test in
  `permissions::tests::serialises_as_pipe_separated_string`.
- `bson::oid::ObjectId`'s default serde emits bson-extended JSON
  (`{"$oid":"..."}`) even in serde_json. Use the helpers in
  `roomler_ai_remote_control::serde_helpers` for every wire field.
- The WS path's `rc:*` dispatch is keyed on `msg.t` starting with
  `"rc:"`; the existing `{type, data}` envelope dispatcher is
  untouched. `ws.ts`'s `sendRaw` and `onRcMessage` are the correct
  frontend entry points.
- `cargo test -p roomler-ai-tests` currently shows 5 pre-existing
  failures unrelated to anything in this session (CORS config with
  tower-http 0.6, one role test's dedup assertion, one rate-limit
  timing flake). Confirmed by running on pristine master before the
  session started. Don't chase them until the rest is stable.

---

## Contact-with-reality plan when you resume

My honest guess is that on the deployment server the first
`./target/release/roomler-agent run` against a live API will fail at
one of:

1. TURN creds endpoint returns no ice_servers (solution: set
   `ROOMLER__TURN__SHARED_SECRET`, or accept STUN-only).
2. The browser's PeerConnection goes `connected` but no video
   arrives (solution: check the agent's logs — `RUST_LOG=roomler_agent=debug`
   — for capture/encode errors; the pump silently logs-and-continues
   on per-frame failures).
3. H.264 profile-level mismatch between browser and agent (solution:
   check `media_engine.register_default_codecs()` in `peer.rs`; may
   need to pin a specific H.264 profile in `RTCRtpCodecCapability`).
4. Input events flow but clicks don't land on the right pixel
   (solution: the agent's `to_pixels` uses `enigo.main_display()` —
   on multi-monitor this may be wrong; temporarily hardcode screen
   dims to debug).

Don't over-engineer around these upfront. Build, run, observe, fix
one at a time.

---

*Last updated: 2026-04-17, commit `68d5e57`.*
