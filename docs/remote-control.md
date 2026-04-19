# Remote Control — Architecture & Design

> Adds TeamViewer / RustDesk-style unattended remote desktop access to roomler-ai, reusing the existing mediasoup SFU, WebSocket signaling, and COTURN cluster. Targets sub-150 ms input-to-glass latency on LAN, sub-300 ms over WAN.

## 1. Goals & Non-Goals

**Goals**

- View and fully control a registered remote machine (Windows / macOS / Linux) from any modern browser, no client install on the controller side.
- One unified room model: a "remote control session" is just a special room kind, so it inherits auth, multi-tenancy, RBAC, presence, chat, recording, and notifications for free.
- Prefer P2P via WebRTC when ICE allows; fall back to TURN over the existing COTURN cluster; never proxy raw input through the application server.
- Multi-monitor, clipboard, file transfer, and a "view-only" guest mode out of the box.
- Audit everything (session start/stop, input events optional, file transfers, clipboard direction).

**Non-goals (v1)**

- Headless/SYSTEM-level UAC elevation prompts on Windows. v1 runs in the user session only.
- Wake-on-LAN / boot-time access. The agent comes up with the user session.
- Mobile agents (iOS/Android as the controlled device). Mobile as controller is fine — it's just a browser.
- Rendering an X11 server's login greeter on Linux. Wayland-first, X11-fallback, both inside an active session.

## 2. Where this fits in the existing stack

The current stack already gives us 80% of what's needed:

| Existing piece | What we reuse |
|---|---|
| Rust + Axum 0.8 backend | New `crates/remote_control` for session/agent state, plus routes module |
| MongoDB + 18 collections | New collections: `agents`, `remote_sessions`, `remote_audit` |
| WebSocket handler (presence, signaling) | New message namespace `rc:*` for agent registration and control signaling |
| mediasoup 0.20 SFU + WorkerPool | New router kind `RemoteControlRouter` with one video producer (screen) + bidirectional `SCTP` data channels |
| COTURN cluster (`coturn.roomler.live`) | Same TURN credentials path; agent fetches short-lived creds from the API |
| JWT + httpOnly cookies | Agent uses a long-lived **agent token** (separate audience claim); controllers use the existing user JWT |
| Vue 3 + Pinia + mediasoup-client | New view `RemoteControl.vue`, new store `remoteControl.ts`, new composable `useRemoteControl.ts` |
| Notifications | "X requested control of your machine" goes through the existing notification bell |

The only genuinely new component is the **native agent** (a separate Rust binary that ships per-OS) and a thin signaling extension on the server.

## 3. High-level topology

```
  ┌──────────────────────────┐                                     ┌────────────────────────────┐
  │  Controller (browser)    │                                     │  Controlled host (agent)   │
  │  Vue 3 + mediasoup-client│                                     │  Rust binary (Tauri tray)  │
  │                          │                                     │  scrap | wgpu | enigo      │
  │  ┌─────────────────────┐ │                                     │  ┌─────────────────────┐   │
  │  │ <video> screen      │◄┼──── RTP H.264 / AV1 (recv) ────────►│  │ capture+encode loop │   │
  │  │ DC: input  (ULR)    │─┼──── SCTP ord/unord ────────────────►│  │ enigo input service │   │
  │  │ DC: control (rel)   │◄┼──── SCTP reliable ─────────────────►│  │ control state       │   │
  │  │ DC: clipboard (rel) │◄┼──── SCTP reliable ─────────────────►│  │ clipboard sync      │   │
  │  │ DC: filexfer (rel)  │◄┼──── SCTP reliable ─────────────────►│  │ chunked file xfer   │   │
  │  └─────────────────────┘ │         (P2P or via TURN)           │  └─────────────────────┘   │
  └────────────┬─────────────┘                                     └────────────┬───────────────┘
               │                                                                │
               │ WSS  (signaling: SDP-equivalent, ICE, session control)         │ WSS
               │                                                                │
               └─────────────────────► Roomler API (Axum) ◄─────────────────────┘
                                       ├─ rc_signaling::Hub
                                       ├─ mediasoup RemoteControlRouter
                                       └─ MongoDB (sessions, agents, audit)

                                              ▲
                                              │ TURN credentials (REST API)
                                              │
                                       coturn.roomler.live
```

The application server **never sees raw input or pixels** — those flow over the WebRTC PeerConnection between agent and controller, either P2P or relayed by TURN. The server only does signaling, authorization, mediasoup routing setup, and audit logging.

## 4. The agent

A standalone Rust binary, distributed as `roomler-agent` per OS. Two operating modes:

1. **Attended** — user runs `roomler-agent --pair` from the tray, gets a one-time PIN, types it in the controller UI. PIN is good for 10 minutes, single use.
2. **Unattended** — agent registers once with an `enrollment_token` (issued by an org Admin via the existing org settings UI), persists a per-machine `agent_token`, and stays connected to the API via WebSocket whenever the user is logged in.

### 4.1 Why a native binary (and not just `getDisplayMedia` + browser)

`getDisplayMedia` works for *attended screen sharing* (which roomler already has via `produce`). It cannot:

- run when no browser tab is open,
- inject mouse/keyboard into other apps,
- read/write the system clipboard,
- access multiple displays distinctly,
- enumerate windows for window-only sharing,
- bypass DRM-protected surfaces with hardware capture paths,
- survive browser tab crashes.

So: native agent for the *host*, browser for the *controller*. This is the same split RustDesk uses, and Chrome Remote Desktop, and Parsec.

### 4.2 Agent crate layout

```
agents/roomler-agent/
├── Cargo.toml                 # workspace member of the main repo
├── src/
│   ├── main.rs                # tray/CLI entry
│   ├── config.rs              # ~/.config/roomler-agent/config.toml
│   ├── enrollment.rs          # one-shot enrollment → agent_token
│   ├── signaling.rs           # WSS to roomler API, rc:* protocol
│   ├── peer.rs                # webrtc-rs PeerConnection wrapper
│   ├── capture/
│   │   ├── mod.rs             # trait ScreenCapture
│   │   ├── windows.rs         # WGC (Windows.Graphics.Capture)
│   │   ├── macos.rs           # ScreenCaptureKit
│   │   ├── linux_wayland.rs   # PipeWire via xdg-desktop-portal
│   │   └── linux_x11.rs       # XShm fallback
│   ├── encode/
│   │   ├── mod.rs             # trait VideoEncoder
│   │   ├── nvenc.rs           # NVIDIA HW
│   │   ├── amf.rs             # AMD HW
│   │   ├── qsv.rs             # Intel HW
│   │   ├── vt.rs              # macOS VideoToolbox
│   │   ├── mediafoundation.rs # Windows MF
│   │   ├── vaapi.rs           # Linux VA-API
│   │   └── openh264.rs        # SW fallback
│   ├── input/
│   │   ├── mod.rs             # trait InputInjector
│   │   ├── enigo_backend.rs   # default (uxn-cross-platform)
│   │   ├── windows.rs         # SendInput (handles UIPI / DPI)
│   │   ├── macos.rs           # CGEventPost (needs Accessibility)
│   │   └── linux.rs           # uinput (Wayland) / XTest (X11)
│   ├── clipboard.rs           # arboard + change watcher
│   ├── filexfer.rs            # chunked, resumable
│   ├── permissions.rs         # OS-specific consent dance
│   └── audit.rs               # local log + push events to server
└── installer/
    ├── windows.wxs            # MSI; auto-start at user login
    ├── macos.plist            # LaunchAgent (user, not Daemon, in v1)
    └── linux/
        ├── roomler-agent.service   # systemd --user
        └── flatpak/...
```

`webrtc-rs` is the right peer-connection lib here — it's pure Rust, has matured a lot, and integrates cleanly with `tokio`. Using it on the agent side means a controller browser sees a regular WebRTC peer; the agent does not need to attach to mediasoup, it just dials the controller's `mediasoup-client` peer.

### 4.3 Why P2P, not always SFU

For 1↔1 remote control, mediasoup as a relay buys you *nothing* and adds a hop. So the agent and controller form a **direct PeerConnection** (P2P with ICE → TURN fallback). The mediasoup SFU only enters the picture when **multiple controllers** observe the same session (e.g., a support session shadowed by a senior engineer, or screen-sharing a remote-control session into a roomler call). In that case, the agent's stream is republished to a mediasoup `RemoteControlRouter` and other controllers consume from the SFU as view-only.

```
1 controller : 1 agent           → direct PeerConnection (best path)
N controllers : 1 agent          → agent → mediasoup → N consumers (view-only)
1 active controller + N watchers → split: 1 PC for input controller, SFU for watchers
```

This hybrid is exactly the `WorkerPool + RoomManager` pattern already in roomler — we just add a transport kind for "remote screen capture" and a one-producer router shape.

## 5. Capture & encode pipeline

### 5.1 Capture targets per OS

| OS | Primary API | Why | Fallback |
|---|---|---|---|
| Windows 10+ | `Windows.Graphics.Capture` (WGC) via `windows` crate | DXGI surface, no permission prompt for own session, handles DPI, supports per-window | DXGI Desktop Duplication |
| macOS 12.3+ | `ScreenCaptureKit` | Apple's blessed path; handles privacy indicators, multi-display | `CGDisplayStream` (deprecated but works) |
| Linux Wayland | `xdg-desktop-portal` ScreenCast → PipeWire | The only sanctioned route on Wayland; works on GNOME, KDE, Sway | None — Wayland refuses raw access |
| Linux X11 | `XShm` + `XCompositeNameWindowPixmap` | Zero-copy via shared memory | Generic XGetImage |

The portal/SCK paths produce a system permission prompt the *first* time. That's a feature, not a bug — it's the user consent layer.

### 5.2 Encoder selection

Picked at agent startup, redetected on GPU change:

```
priority order:
  1. HW: nvenc | qsv | amf | vaapi | videotoolbox | mediafoundation
  2. SW: openh264 (always available)
codec preference:  AV1  > H.265  > H.264
```

H.264 is the safe default — every browser decodes it. AV1 only if the controller's `RTCRtpReceiver.getCapabilities('video')` advertises it. We negotiate this in the SDP exchange.

### 5.3 Adaptive streaming

- Two-layer simulcast in SW (one full-res, one half-res) so the controller can switch instantly on bandwidth dips.
- `goog-remb` / TWCC feedback drives target bitrate; reasonable bounds: 600 kbps (idle) → 25 Mbps (4K motion).
- **Variable framerate, idle skip**: when nothing on screen changes (hashed via dirty-rect tracking from WGC/SCK), the encoder emits a 1 fps keepalive instead of a full 60 fps stream. This is the single biggest battery and bandwidth win.
- IDR on demand: the controller sends `{"t":"keyframe"}` over the control DC if it detects decode errors after a packet loss spike; agent issues an immediate keyframe.

## 6. Input injection

Three input planes, all over a single SCTP-unreliable data channel labeled `input`:

```rust
// shared schema, serialized as msgpack (smaller + faster than JSON)
#[derive(Serialize, Deserialize)]
#[serde(tag = "t")]
enum InputMsg {
    MouseMove   { x: f32, y: f32, mon: u8 },        // normalized 0..1 per monitor
    MouseButton { btn: Button, down: bool, x: f32, y: f32, mon: u8 },
    MouseWheel  { dx: f32, dy: f32, mode: WheelMode },
    Key         { code: u32, down: bool, mods: u8 }, // USB HID usage code
    KeyText     { text: String },                    // for IME / unicode
    Touch       { id: u32, phase: TouchPhase, x: f32, y: f32 },
    Heartbeat   { seq: u64, ts_ms: u64 },
}
```

Key design choices:

- **Normalized coordinates** (0..1 per monitor index), not pixels. The browser doesn't know the agent's resolution, and the agent's resolution can change mid-session (laptop docking, etc.). The agent maps to absolute pixels using its current monitor geometry.
- **HID usage codes**, not browser key codes or X11 keysyms. Browser keyboard events expose `KeyboardEvent.code` which maps cleanly to HID; the agent maps HID → OS-native scan codes. This is the only way to get layout-independent behavior (a German controller pressing the physical "Z" key sends "Y" on a US-layout host *correctly*, because the host's layout interprets the scan code).
- **Unreliable DC, ordered=false, maxRetransmits=0** — input is real-time; a dropped move event is replaced by the next one a few ms later. Latency >> reliability.
- **Mouse coalescing on the controller**, not the agent. The browser fires `pointermove` at up to display refresh rate; we coalesce to one msg per RAF (~16 ms), preserving the *last* position but dropping intermediate samples. Click/key events are never coalesced.
- **`enigo` is the default**, with OS-specific direct backends behind a feature flag for performance and edge cases (UIPI on Windows, IME composition on macOS, `uinput` on Wayland because XTest doesn't exist there).

### 6.1 The Wayland problem

Wayland has no equivalent of XTest. The supported path is `/dev/uinput`, which requires the agent to be in the `input` group (or have `CAP_SYS_ADMIN`). Installer adds the user to `input` group + udev rule. If permission isn't granted, the agent runs in **view-only** mode and the UI clearly says so.

### 6.2 Remote cursor

The agent does **not** render the cursor into the captured frame (it tells the OS "I'm capturing, hide the cursor"). Instead, it sends cursor shape + position over the `control` DC. The controller renders the cursor as a CSS overlay on top of the video. This eliminates the "delayed mouse" feeling that plagues lower-end remote desktop tools — the local cursor moves at native refresh, the video catches up.

## 7. Signaling protocol (`rc:*` namespace)

Extension to the existing WebSocket. All messages are JSON envelopes; the existing `WS Handler` routes by prefix.

### 7.1 Agent → server

```jsonc
// on connect, after WSS auth via agent_token
{"t":"rc:agent.hello", "machine":"goran-9950x3d", "os":"linux", "displays":[...], "caps":{...}}

// answer to a controller's offer
{"t":"rc:sdp.answer",  "session":"sess_abc", "sdp":"..."}
{"t":"rc:ice",         "session":"sess_abc", "candidate":"..."}

// session-control replies
{"t":"rc:consent",     "session":"sess_abc", "granted":true}

// passive
{"t":"rc:agent.heartbeat", "rss_mb":124, "fps":58, "encoder":"nvenc-h264"}
```

### 7.2 Server → agent

```jsonc
{"t":"rc:request",   "session":"sess_abc", "controller":{"user_id":"u_1","name":"Goran"}, "permissions":["input","clipboard","files"]}
{"t":"rc:offer",     "session":"sess_abc", "sdp":"...", "ice_servers":[{"urls":"turn:..."}]}
{"t":"rc:ice",       "session":"sess_abc", "candidate":"..."}
{"t":"rc:terminate", "session":"sess_abc", "reason":"user_disconnect"}
```

### 7.3 Controller browser ↔ server

Same `rc:*` shapes; the controller is just the other peer. The server is a relay only for SDP/ICE, never for media.

### 7.4 Why not piggyback on mediasoup signaling

mediasoup-client speaks its own RPC for `transport.connect/produce/consume`. That's the right protocol when mediasoup is in the path, but for direct P2P agent↔controller it's overkill. We'd be paying for a router roundtrip just to swap SDP. So: a thin custom signaling layer for the 1:1 case, mediasoup signaling for the N-watcher case.

## 8. Data model additions

```rust
// crates/data/src/models/agent.rs
pub struct Agent {
    pub id: ObjectId,
    pub org_id: ObjectId,
    pub owner_user_id: ObjectId,
    pub name: String,                  // user-friendly: "Goran's Laptop"
    pub machine_id: String,            // stable hardware fingerprint (HMAC of dmi+mac)
    pub os: OsKind,
    pub agent_version: String,
    pub agent_token_hash: String,      // argon2 of the long-lived token
    pub status: AgentStatus,           // Online | Offline | Unenrolled
    pub last_seen_at: DateTime,
    pub displays: Vec<DisplayInfo>,    // refreshed on every connect
    pub capabilities: AgentCaps,       // hw encoders, has_input_perm, etc.
    pub access_policy: AccessPolicy,   // who from this org can request control
    pub created_at: DateTime,
}

pub enum AgentStatus { Online, Offline, Unenrolled, Quarantined }

pub struct AccessPolicy {
    pub require_consent: bool,         // user must click "Allow" each time
    pub allowed_role_ids: Vec<ObjectId>,
    pub allowed_user_ids: Vec<ObjectId>,
    pub auto_terminate_on_idle_min: Option<u32>,
}

// crates/data/src/models/remote_session.rs
pub struct RemoteSession {
    pub id: ObjectId,
    pub agent_id: ObjectId,
    pub org_id: ObjectId,
    pub controller_user_id: ObjectId,
    pub watchers: Vec<ObjectId>,       // view-only participants
    pub permissions: Permissions,      // input, clipboard, files, audio
    pub started_at: DateTime,
    pub ended_at: Option<DateTime>,
    pub end_reason: Option<EndReason>,
    pub recording_url: Option<String>, // optional; recorded as standard mediasoup recording
    pub stats: SessionStats,           // bytes, peak fps, avg rtt
}

// crates/data/src/models/remote_audit.rs
pub struct RemoteAuditEvent {
    pub session_id: ObjectId,
    pub at: DateTime,
    pub kind: AuditKind,               // Started, ConsentGranted, ClipboardWrite, FileSent, ...
    pub detail: Bson,
}
```

Indexes (typical):
- `agents`: `{org_id:1, status:1}`, `{owner_user_id:1}`, `{machine_id:1}` unique per org
- `remote_sessions`: `{agent_id:1, started_at:-1}`, `{controller_user_id:1, started_at:-1}`
- `remote_audit`: `{session_id:1, at:1}` + TTL on `at` for org retention policy

## 9. New backend crates / modules

```
crates/
├── remote_control/
│   ├── src/
│   │   ├── lib.rs
│   │   ├── hub.rs            # registry: agent_id → WS, session_id → state
│   │   ├── session.rs        # state machine: Pending → AwaitingConsent → Active → Closed
│   │   ├── signaling.rs      # rc:* message routing
│   │   ├── consent.rs        # consent prompt + timeout
│   │   ├── permissions.rs    # what a controller is allowed to do this session
│   │   ├── sfu_bridge.rs     # publishes agent stream into mediasoup for N-watcher case
│   │   ├── turn_creds.rs     # REST API short-lived TURN creds (HMAC over coturn shared secret)
│   │   └── audit.rs
│   └── Cargo.toml
├── routes/
│   └── src/
│       └── remote_control.rs # /api/agents, /api/agents/:id/sessions, /api/agents/enroll
└── server/
    └── src/
        └── ws/
            └── rc.rs         # rc:* dispatcher → remote_control::hub
```

### 9.1 REST surface

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/api/agents/enroll-token` | Admin creates a one-shot enrollment token (returns QR + CLI command) |
| `POST` | `/api/agents/enroll` | Agent exchanges enrollment token for `agent_token` |
| `GET` | `/api/agents` | List agents in current org (filtered by RBAC) |
| `GET` | `/api/agents/:id` | Agent detail incl. live status, displays |
| `PATCH` | `/api/agents/:id` | Rename, update access policy |
| `DELETE` | `/api/agents/:id` | Revoke (server-side token blacklist) |
| `POST` | `/api/agents/:id/sessions` | Request a new session (returns `session_id`; SDP exchange happens over WS) |
| `GET` | `/api/sessions/:id` | Session detail + live stats |
| `POST` | `/api/sessions/:id/terminate` | Force-end (controller, agent owner, or org admin) |
| `GET` | `/api/sessions/:id/audit` | Audit trail |
| `GET` | `/api/turn/credentials` | Short-lived TURN creds for browser & agent |

### 9.2 Hub state machine

```
                      consent.timeout (30s)
        ┌─────────────────────────────────────┐
        ▼                                     │
  ┌──────────┐  request   ┌────────────────┐  │  granted   ┌─────────┐
  │ Pending  │──────────► │AwaitingConsent │──┴──────────► │ Active  │
  └──────────┘            └────────────────┘  denied        └────┬────┘
                                              ─────────►        │ ws_drop / terminate
                                              Rejected           ▼
                                                            ┌─────────┐
                                                            │ Closed  │
                                                            └─────────┘
```

A session is the only thing that can hold a mediasoup transport open in this subsystem; closing it tears down the routers and frees worker slots.

## 10. Frontend additions

```
ui/src/
├── views/
│   ├── Agents.vue                 # list + status per agent
│   └── RemoteControl.vue          # the actual session view
├── stores/
│   └── remoteControl.ts           # Pinia store
├── composables/
│   └── useRemoteControl.ts        # PeerConnection lifecycle, DC handlers
└── components/
    └── remote/
        ├── ScreenCanvas.vue       # <video> + cursor overlay + input handlers
        ├── MonitorPicker.vue
        ├── ToolBar.vue            # Ctrl-Alt-Del send, file, clipboard, quality
        ├── FileTransferPanel.vue
        └── ParticipantsBar.vue    # for multi-watcher sessions
```

`RemoteControl.vue` does the gnarly browser-side work:

- Captures `pointermove` / `pointerdown` / `wheel` / `keydown` / `keyup` on a focused, cursor-hidden surface.
- For mouse: requests pointer lock when entering "fullscreen control" mode so movement is unbounded.
- For keyboard: uses `KeyboardEvent.code` (HID-aligned), traps the browser's reserved combos with `navigator.keyboard.lock(['Tab','Escape',...])`. This is a real API and is how Discord/Parsec keep Tab/Esc from leaving the page.
- Coalesces mouse events to RAF cadence, sends keys immediately.
- Renders cursor shape + position from the `control` DC as an SVG overlay.

```ts
// useRemoteControl.ts (sketch)
export function useRemoteControl(sessionId: string) {
    const pc = new RTCPeerConnection({ iceServers: await fetchTurnCreds() });
    const inputDc    = pc.createDataChannel('input',     { ordered: false, maxRetransmits: 0 });
    const controlDc  = pc.createDataChannel('control',   { ordered: true });
    const clipDc     = pc.createDataChannel('clipboard', { ordered: true });
    const fileDc     = pc.createDataChannel('files',     { ordered: true });

    pc.ontrack = e => videoEl.srcObject = e.streams[0];

    // Signaling via existing WS
    ws.on('rc:offer',   ({ sdp }) => pc.setRemoteDescription({type:'offer',sdp})
                                       .then(() => pc.createAnswer())
                                       .then(a => { pc.setLocalDescription(a); ws.send({t:'rc:sdp.answer', sdp:a.sdp})}));
    pc.onicecandidate = e => e.candidate && ws.send({t:'rc:ice', candidate:e.candidate.toJSON()});

    return { pc, inputDc, controlDc, clipDc, fileDc };
}
```

## 11. Auth, consent, security

### 11.1 Tokens

| Token | Audience | Lifetime | Storage |
|---|---|---|---|
| User JWT | `aud=user` | 30 min, refresh 30 d | httpOnly cookie (existing) |
| Enrollment token | `aud=enroll`, `org_id`, `single_use=true` | 10 min | shown once in UI, copied to agent CLI |
| Agent token | `aud=agent`, `agent_id`, `org_id` | 1 year, rotates on use | argon2-hashed in `agents`, raw in agent's OS keychain |
| Session token | `aud=session`, `session_id`, perms[] | session duration | in-memory both sides |
| TURN creds | HMAC-SHA1 over coturn shared secret | 10 min | not stored |

### 11.2 Consent

Even with full unattended permission granted by org policy, the controlled user can be configured to see a non-blocking toast: *"Goran is requesting control. [Allow] [Deny] (auto-deny in 25s)"*. Default for org members controlling org devices: **prompt every session**. Default for self-controlling-self (e.g., your laptop from your phone): **no prompt**, owner identity proven by JWT.

### 11.3 Recording & audit consent

If recording is enabled for the session, the controlled side gets a persistent banner (cannot be dismissed) and a red dot in the tray icon. Mirrors macOS's screen-recording indicator behavior; users have learned to look for it.

### 11.4 The reality of misuse

A remote-control feature is the most-abused capability in any product. Mitigations:

- **No silent install**. The agent installer always shows a consent screen and creates a tray icon.
- **Quarantine flag**: org admins can mark an agent quarantined, which blocks new sessions but keeps the agent registered.
- **Geofencing & impossible-travel**: log controller IP geo per session, surface anomalies in admin UI.
- **Mandatory audit retention** is configurable per-org but cannot go below 30 d for sessions.
- **No keystroke logging** in audit — only event counts. The controlled user's passwords typed during a session must not be persisted.
- **Tray icon cannot be hidden** by config; if you want covert monitoring, this is the wrong product.

## 12. Performance targets & budget

| Stage | Target (LAN) | Target (WAN, RTT 30 ms) |
|---|---|---|
| Capture (frame ready → encoder in) | 2 ms | 2 ms |
| Encode (HW H.264, 1080p) | 4 ms | 4 ms |
| Network out → in (incl. TURN if any) | 5 ms | 50 ms |
| Decode (browser) | 5 ms | 5 ms |
| Composite + display | 16 ms (1 vsync at 60 Hz) | 16 ms |
| Input event → server-side queue | 0 (P2P) | 30 ms |
| Input injection → next frame captured | up to 16 ms | up to 16 ms |
| **End-to-end glass-to-glass** | **~50 ms** | **~120-180 ms** |

Two practical observations:

1. The single biggest WAN latency contributor is TURN relaying through a far-away region. Co-locate TURN with users; roomler already runs `coturn.roomler.live` — for a global rollout, deploy regional TURN endpoints and let the agent pick the lowest-RTT one at registration.
2. The single biggest CPU/battery contributor is encoding *unchanged frames*. Dirty-rect skipping is non-negotiable.

## 13. Testing strategy

Reuse the existing 114 Rust integration test conventions plus Playwright E2E specs. New harness pieces:

- **`agent-headless`** test binary: a stripped-down agent that captures from a virtual framebuffer (Xvfb on Linux CI) and accepts injected input via stdin. Lets us assert end-to-end round trips in CI without a GPU.
- **Latency probe**: a Playwright test that draws a known pattern, sends a click, and asserts the agent received the click within budget.
- **Loss simulation**: `tc netem` in the test container to add 100 ms RTT, 2% loss, and verify the input/control DC behave correctly.
- **Multi-watcher SFU bridge**: spin up 3 mediasoup consumers against one agent producer, verify CPU stays bounded.

## 14. Rollout plan (suggested phases)

| Phase | Scope | Calendar guess (solo) |
|---|---|---|
| **0. Spike** | webrtc-rs PoC on Linux: capture+encode → browser, view-only, no input | 1 week |
| **1. MVP** | Linux-X11 agent + browser controller, attended PIN pairing, mouse + keyboard, single monitor, no SFU bridge | 3 weeks |
| **2. Productize** | Windows + macOS agent, unattended enrollment, multi-monitor, clipboard, consent UI, audit | 5 weeks |
| **3. Scale** | SFU bridge for N-watchers, file transfer, recording, hardware encoders on all platforms, regional TURN | 4 weeks |
| **4. Polish** | Wayland, AV1, mobile-controller UX, RBAC integration with existing roles bitfield, installer signing | 3 weeks |

That's ~16 weeks for a properly hardened v1, which is in the right ballpark for what RustDesk took to mature.

## 15. Decisions worth flagging

- **`webrtc-rs` over wrapping libwebrtc** — pure Rust toolchain matches the rest of roomler, avoids a C++ build dependency, and the API surface we need (PC + DC + RTP send) is well-supported. The trade-off is fewer mature codec integrations; we route around that by encoding ourselves and feeding raw H.264 NALUs into a `TrackLocalStaticSample`.
- **Tauri for the tray, not Electron** — keeps the agent under 20 MB and the dependency tree close to the rest of your stack.
- **`enigo` as default, OS-specific only when needed** — same calculus RustDesk made; enigo handles 90% well, the remaining 10% (Wayland, IME, UIPI) needs direct backends.
- **One unified room kind, not a separate "remote" service** — keeps notifications, RBAC, presence, chat, and recording free. The cost is one new `RoomKind::RemoteControl` variant and the discipline to keep `remote_control` crate's surface narrow.
- **No SOCKS-style port forwarding in v1** — RustDesk has it; it's a security minefield and 90% of users don't need it. Add later if there's demand.

## 16. Open questions

1. Do you want to allow **headless agents** (no logged-in user, e.g., a server in a rack)? That requires a system service, which is a much bigger blast radius. Recommend deferring past v1.
2. Should an in-progress remote control session be **shareable into a roomler call** as a screen share automatically? The plumbing supports it; it's a UX call.
3. **Recording storage**: piggyback on the existing MinIO setup, or push to S3-compatible per-org bucket? Existing MinIO is fine for v1.
4. **Mobile controller** keyboard UX is genuinely hard (no physical keys, lots of host-OS shortcuts to send). v1 should be view + tap-to-click only on mobile, full input on desktop browsers.

## 17. Hardware encoder backends

### Current state (0.1.25)

On Windows, the default `Auto` cascade picks **openh264** — the
software H.264 encoder we've trusted since day one. The Windows
Media Foundation backend (`mf-h264`) is compiled in and functional
but **opt-in only** via `encoder_preference=hardware`, because on
mixed-GPU hosts (e.g. NVIDIA GeForce + Intel iGPU) it hits two
blockers that phase 3 will resolve:

1. **NVIDIA H.264 Encoder MFT** `ActivateObject` returns
   `0x8000FFFF` when the D3D11 device is bound to the default DXGI
   adapter (usually the Intel iGPU on hybrid laptops / desktops
   with both). NVENC MFT requires its D3D device to be created on
   NVIDIA's adapter specifically.
2. **Intel Quick Sync Video H.264 Encoder MFT** activates OK but
   is async-only. It ignores `MF_TRANSFORM_ASYNC_UNLOCK` and
   rejects `SET_D3D_MANAGER` with `MF_E_ATTRIBUTENOTFOUND`. Sync
   `ProcessOutput` returns `0x8000FFFF` on the first drain.

Phase 3 (DXGI adapter enumeration + `IMFMediaEventGenerator`
event loop + per-MFT probe-and-rollback) is a separate focused
work package. Until it lands, Auto → openh264 is the right call.

### Backend cascade

`encode::open_default(width, height, preference)` picks as follows:

| `preference` | Order tried |
|---|---|
| `Auto` (default) | **openh264** → Noop   (MF is opt-in until phase 3) |
| `Hardware` | Windows MF (experimental) → openh264 → Noop |
| `Software` | openh264 → Noop |

Selection is logged at INFO:

    INFO encoder selected: openh264 (software) width=1920 height=1080

and on every `media pump heartbeat`:

    INFO media pump heartbeat backend="openh264" frames_encoded=30 ...

### Per-session downscale behaviour

The capture layer runs a 2× box downsample on sources above
~3.5 Mpx when the active encoder is software (openh264 or MF SW).
Hardware encoders (when phase 3 lands) will skip the downsample so
they see native resolution. Logged at pump start:

    INFO media pump starting encoder_preference=Auto downscale=Auto

### Configuration

Three places, in decreasing priority:

1. **CLI flag**: `roomler-agent run --encoder hardware` (also
   accepts `auto`, `software`, `hw`, `sw`, `mf`, `openh264`).
2. **Env var**: `ROOMLER_AGENT_ENCODER=hardware`. Mostly for
   systemd-user / Task Scheduler entries where editing the TOML is
   less convenient.
3. **Config file** (`config.toml`): `encoder_preference = "hardware"`.

Invalid values fall through to `Auto` with a warning — a typo can
never prevent the agent from starting.

### Known hardware issues (to fix in phase 3)

Verification priority: NVIDIA → Intel iGPU → AMD.

| Vendor | Driver | Symptom | Workaround |
|---|---|---|---|
| NVIDIA (GTX 1650 + Intel UHD 630 mixed) | 560.x series | `NVIDIA H.264 Encoder MFT` `ActivateObject` returns `0x8000FFFF` (E_UNEXPECTED) because D3D11 device was created on the default adapter (Intel). Fix requires DXGI adapter enumeration + VendorId=0x10DE match. | Use `encoder_preference=software` (default) |
| Intel UHD 630 iGPU (Quick Sync) | same | `Intel® Quick Sync Video H.264 Encoder MFT` is async-only; ignores `ASYNC_UNLOCK`; first sync `ProcessOutput` returns `0x8000FFFF`. Fix requires `IMFMediaEventGenerator` event loop. | Use `encoder_preference=software` (default) |
| AMD | *(not yet tested)* | expected to behave like Intel QSV (async) | |

### Encoder smoke test

Release builds run `roomler-agent encoder-smoke --encoder hardware`
as part of the Windows CI job. It opens the preferred encoder at
640×480, feeds 10 synthetic frames, and fails the build if no
keyframe comes out or the cascade bottoms at `NoopEncoder`.

To reproduce locally:

    cargo build -p roomler-agent --release --features full-hw
    target\release\roomler-agent.exe encoder-smoke --encoder hardware

With the `full-hw` build, the MF backend code is present but only
engaged when `--encoder hardware` is passed explicitly.

### Scaffolding already in place

The following phase-1-and-2 plumbing stays in the codebase ready
to be re-engaged once phase 3 adds the missing pieces:

- `create_d3d11_device_and_manager()` — builds a multithread-
  protected D3D11 device + `IMFDXGIDeviceManager`. Works but binds
  to default adapter.
- `activate_h264_encoder()` — `MFTEnumEx` with
  `MFT_ENUM_FLAG_HARDWARE | SORTANDFILTER | SYNCMFT`. Returns
  first-activating vendor MFT, falls back to MS SW.
- Async-mode probe via `GetAttributes().GetUINT32(MF_TRANSFORM_ASYNC)`
  + `MF_TRANSFORM_ASYNC_UNLOCK` attempt. Works for MFTs that honour
  unlock; doesn't for those that don't.
- `MFT_MESSAGE_SET_D3D_MANAGER` handoff, tolerant of rejection.
- `MF_E_TRANSFORM_STREAM_CHANGE` handling in the drain loop.
- Debug tracing at every `ProcessInput`/`ProcessOutput`.

### Phase 3 scope

Three pieces, each tractable on its own:

1. **DXGI adapter enumeration** — ✅ landed in 0.1.26 commit 2
   (`encode/mf/adapter.rs`). `CreateDXGIFactory1` →
   `EnumAdapters1` → vendor priority rank → `D3D11CreateDevice` on
   that specific adapter. The cascade then feeds the adapter-bound
   device to each enumerated H.264 MFT.

2. **Async event loop** — ⏳ commit 1A.2 (tracked; not yet needed
   on the RTX 5090 Laptop + AMD box since both vendors' MFTs
   honour `MF_TRANSFORM_ASYNC_UNLOCK`). Design: `QueryInterface`
   for `IMFMediaEventGenerator`, dedicated worker thread running
   `GetEvent` (blocking) → `METransformNeedInput` pulls the next
   input from an mpsc queue and calls `ProcessInput` →
   `METransformHaveOutput` calls `ProcessOutput` and pushes to
   another mpsc. `VideoEncoder::encode()` becomes a non-blocking
   pusher that drains available outputs. Intel QSV is the main
   target; cascade routes candidates that ignore `ASYNC_UNLOCK`
   to `MfInitError::AsyncRequired` today and logs them.

3. **Per-MFT probe-and-rollback** — ✅ landed in 0.1.26 commit
   1A.1 (`encode/mf/activate.rs` + `encode/mf/probe.rs`). Full
   pipeline init + one 480×270 NV12 black-frame probe per
   candidate; non-zero output within the existing 64-iteration
   drain cap is required. Additional hardening beyond the
   original scope: blanket `MF_TRANSFORM_ASYNC_UNLOCK` regardless
   of the reported flag (the MS SW MFT silently delegates to
   async HW and reports `is_async=false`), and tolerance for
   `SET_D3D_MANAGER E_NOTIMPL` (treats the candidate as a sync
   CPU MFT with no D3D binding — matches the "H264 Encoder MFT"
   entry that `MFTEnumEx` returns for the MS SW MFT).

Status: **Phase 3 commits 1 + 2 + 3 landed**. Auto cascade on
Windows now prefers MF-HW (commit 1A.3, 0.1.26) with
`ROOMLER_AGENT_HW_AUTO=0` escape hatch. Async pipeline (commit
1A.2) remains tracked for Intel QSV boxes.

Live verification (2026-04-20, Win11 + RTX 5090 Laptop + AMD
Radeon 610M): cascade enumerates 2 adapters + 5 H.264 MFTs, winner
is AMD Radeon 610M + H264 Encoder MFT, `encoder-smoke --encoder
hardware` produces 1 keyframe + 9 P-frames, total 4212 bytes over
10 frames. `encoder-smoke --encoder auto` picks `mf-h264` unless
`ROOMLER_AGENT_HW_AUTO=0` is set, in which case it picks
`openh264`.

### Future phases beyond Windows

Deferred per platform:

- **macOS**: VideoToolbox `VTCompressionSession`. Sync-ish API,
  per-user `com.apple.security.device.audio-input` entitlement
  should already be covered by the existing signed .pkg build.
- **Linux**: VAAPI via `libva`. Intel + AMD on kernel drivers;
  separate NVENC path for NVIDIA.
- **GPU-side capture → encoder pipeline** (all platforms).
  `CLSID_VideoProcessorMFT` upstream of the encoder MFT on
  Windows so BGRA→NV12 never touches the CPU. A DXGI Desktop
  Duplication capture backend keeps frames as D3D11 textures
  end-to-end — removes the 900 MB/s of memory bandwidth we push
  at native 4K today.
