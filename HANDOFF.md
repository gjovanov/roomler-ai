# Remote Control — Handoff Notes

This drop-in adds the foundation of a TeamViewer-style remote desktop subsystem
to roomler-ai. The full architecture lives in `docs/remote-control.md` — **read
that first**. This file is a short pointer for the next person (or Claude Code)
picking up the work.

## What's in this drop

```
crates/remote_control/          # new Rust crate — signaling + state only
├── Cargo.toml
├── Cargo.lock                   (kept; safe lockfile of deps that compile clean)
└── src/
    ├── lib.rs                   # module wiring + re-exports
    ├── error.rs                 # unified Error / Result
    ├── permissions.rs           # per-session capability bitfield
    ├── models.rs                # Mongo entities (Agent, RemoteSession, Audit)
    ├── signaling.rs             # `rc:*` wire protocol (ClientMsg / ServerMsg)
    ├── consent.rs               # oneshot consent slot w/ timeout
    ├── turn_creds.rs            # coturn REST HMAC-SHA1 credentials
    ├── audit.rs                 # batched Mongo audit writer
    ├── session.rs               # in-memory session state machine
    └── hub.rs                   # process-wide registry + dispatch

docs/remote-control.md           # full architecture (16-week roadmap)
```

## Wiring into roomler-ai

Add the crate to the workspace `Cargo.toml` at the repo root:

```toml
[workspace]
members = [
    # ... existing members ...
    "crates/remote_control",
]
```

## Status

- Compiles cleanly: `cargo check --tests` ✓ on Rust stable
- Unit tests (no DB): consent timeouts, session transitions, TURN creds,
  signaling roundtrip — all green
- Integration tests that exercise `Hub::create_session` → `deliver_consent`
  round-trip are present but require a local MongoDB at `mongodb://localhost:27017`

## What's next (in order)

Follow the phases in `docs/remote-control.md` §14. Concretely, the next four
work items are:

1. **WS handler** — extend the existing `WS Handler` in the main server to
   dispatch `rc:*` messages into `Hub::dispatch`. The handler owns the socket
   pump loop; `Hub` gives you an `mpsc::Receiver<ServerMsg>` per connection
   that you forward to the client.
2. **REST routes module** — `crates/routes/src/remote_control.rs` implementing
   the table in §9.1 of the doc (`/api/agents/*`, `/api/sessions/*`,
   `/api/turn/credentials`). The Mongo CRUD is thin; auth guards reuse the
   existing JWT middleware with a new `aud=agent` claim for the agent token.
3. **Agent binary** — `agents/roomler-agent/` as laid out in §4.2 of the doc.
   `webrtc-rs` + `scrap`/`windows-capture`/`ScreenCaptureKit` for capture,
   `enigo` for input. Start with Linux-X11-only as the MVP (phase 1).
4. **Frontend** — `ui/src/views/RemoteControl.vue` +
   `ui/src/composables/useRemoteControl.ts`. Signaling piggybacks on the
   existing WS connection; the PeerConnection is new and lives in the
   composable.

## Design contracts you shouldn't break

- **The server never sees raw input or pixels.** Those flow over the direct
  WebRTC PC between agent and browser. The server is signaling + policy only.
- **One `ObjectId` per `RemoteSession`.** That id is the cross-cutting key for
  signaling, audit, and billing.
- **`Permissions` is the source of truth on what a controller can do.** The
  agent enforces; the server only transmits the negotiated bitfield.
- **`Hub::dispatch` is the single entry point** from the WS handler. Don't
  sneak around it — the state machine invariants only hold if all messages
  go through `dispatch`.
- **TURN credentials are short-lived (10 min)**. Never embed the coturn
  shared secret in a client. `turn_creds::TurnConfig::issue` is the only
  place that touches the secret.

## Opinions worth re-examining if you disagree

- `webrtc-rs` for the agent instead of wrapping `libwebrtc`. Fits the Rust
  toolchain; trade-off is smaller ecosystem for codec integrations.
- `enigo` as the default input backend with OS-specific backends behind
  feature flags. Same calculus RustDesk made.
- **P2P for 1:1 sessions, mediasoup only for N-watcher sessions.** Keeps
  1:1 latency low; requires a small `sfu_bridge` when N>1.

## Things deliberately NOT in this drop

- The mediasoup SFU bridge (`sfu_bridge.rs`) — only needed for multi-watcher.
- The agent binary skeleton — the doc lays it out, but it'll be ~3 weeks to
  build even the Linux MVP, and it's better to do that with Claude Code
  where you can actually run it.
- The frontend — same reason; hot-reload + browser dev tools matter more
  there than careful up-front design.
- The mongoose-style DAO helpers. The models derive Serialize/Deserialize;
  roomler's existing DAL conventions should wrap them.
