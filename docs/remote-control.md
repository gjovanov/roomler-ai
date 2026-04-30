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

## 18. Viewer controls + codec negotiation + DC handlers (0.1.32 → 0.1.35)

Post-Phase-3 the subsystem grew three feature families, tracked in
`HANDOVER9.md` with full commit-by-commit detail. Summary:

### 18.1 Codec negotiation (0.1.28 → 0.1.30)

Agent advertises H.264 + HEVC + AV1 capabilities via `AgentCaps.codecs`
in `rc:agent.hello`. Browser advertises its decode caps in
`ClientMsg::SessionRequest.browser_caps`. Agent picks the best
intersection with priority `av1 > h265 > vp9 > h264 > vp8` and binds
the matching MF encoder + `video/H264|H265|AV1` track +
`set_codec_preferences` SDP pin. HEVC/AV1 activation failures are
fail-closed (black video + WARN, not silent bitstream substitution).
Caps probe-at-startup (0.1.30) filters codecs that enumerate-but-fail-
to-activate, so the browser never sees a bait-and-switch.

### 18.2 Data-channel handlers (0.1.31 → 0.1.33)

- **Cursor DC** (0.1.31): agent pumps `cursor:pos` + `cursor:shape` at
  ~30 Hz; browser paints the real OS cursor bitmap on an overlay
  canvas. Synthetic initials-badge is the fallback when no shape has
  been cached yet.
- **Clipboard DC** (0.1.32): thread-pinned `arboard::Clipboard` worker;
  JSON protocol `clipboard:read` / `clipboard:write` /
  `clipboard:content` / `clipboard:error` with `req_id` round-trip
  for interleaved reads. Fixed in 0.1.34: `Clipboard` handle had a
  `Drop` impl that sent `Shutdown` on every clone drop, killing the
  worker on the first closure-captured clone release; dropped the
  `Drop` impl and rely on Sender refcount to end the `rx.recv()`
  loop naturally.
- **File DC** (0.1.33): browser drag/pick → `files:begin` →
  64 KiB ArrayBuffer chunks with `bufferedAmount` back-pressure →
  `files:end` → agent writes into the controlled host's Downloads
  folder. Filename sanitization + collision-safe rename + 2 GiB
  per-transfer cap.

### 18.3 Hotkey + viewer indicator (0.1.33)

- **Hotkey interception** in `useRemoteControl.ts::attachInput`:
  Ctrl/Cmd + A/C/V/X/Z/Y/F/S/P/R are locally `preventDefault`-ed
  while the pointer is over the viewer, still forwarded to the
  remote; outside the video the controller keeps normal browser UX.
  `Tab` + bare `Backspace` are globally intercepted.
  Ctrl+Alt+Del is exposed as a dedicated toolbar button — the
  OS reserves the real chord, the browser can't catch it.
- **Viewer-indicator overlay** (`viewer-indicator` feature, Windows
  only): topmost layered click-through window on the controlled
  host with a 6 px red border + "Being viewed by: …" caption.
  `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` keeps the
  overlay out of every screen-capture path (our WGC backend, DXGI
  Desktop Duplication, BitBlt, third-party tools), so the controller
  doesn't see a recursive picture-frame in the RTP stream.

### 18.4 Input fix — Windows VK path (0.1.34)

`hid_to_key` previously mapped letters/digits to `Key::Unicode(c)`.
enigo routes `Key::Unicode` through `KEYEVENTF_SCANCODE` on Windows,
a layout-sensitive path that drops modifier composition on
non-US / International layouts — producing `©` for Ctrl+C and
`^H` for Backspace in pwsh / Windows Terminal. Letters now route
through `Key::Other(VK_A..VK_Z)` and digits through
`Key::Other(VK_0..VK_9)` on Windows only; non-Windows continues to
use `Key::Unicode` because XTest / CGEventPost combine modifiers
with Unicode fine.

### 18.5 RustDesk-parity Tier A (0.1.33)

Shipped to close the `<video>`-based viewer's smoothness gap vs
RustDesk's native client on the HW path. Details in
`~/.claude/plans/floating-splashing-nebula.md`.

- **60 fps + native resolution** on the MF-HW path (`TARGET_FPS_HW=60`,
  `DownscalePolicy::Never` when `mf-encoder` is compiled in).
- **Bitrate ceilings lifted**: 0.10 → 0.15 bpp/s baseline, MAX 15 →
  25 Mbps, High-quality clamp 20 → 30 Mbps.
- **Codec override dropdown** in the UI; persists per browser;
  `filterCapsByPreference` narrows `browser_caps` before the
  `rc:session.request` so the agent can't pick the excluded codec.
- **Browser buffering to zero**: `jitterBufferTarget = 0`,
  `playoutDelayHint = 0`, `contentHint = 'motion'`,
  `requestVideoFrameCallback` keeps the tab hot + `play()` kicker
  rescues idle-optimizer pauses. Chrome still enforces a soft
  ~80 ms JB floor regardless — Tier B7 (WebCodecs canvas render)
  is the deferred structural escape.

### 18.6 Viewer scale + remote resolution (0.1.35)

- **Scale** (`RcScaleMode`): `adaptive` (default, `object-fit:
  contain` fit-to-stage), `original` (1:1 intrinsic pixels with
  scrollbars), `custom` (5-1000% CSS scale). Persisted per browser.
  Input coordinate mapper switches between `letterboxedNormalise`
  (adaptive) and the new `directVideoNormalise` (original/custom)
  so clicks land accurately in every mode. Cursor overlays
  (synthetic badge + real-OS-cursor canvas) are scale-aware via
  `cursorMapping()`.
- **Fullscreen** toggle: `requestFullscreen` on the stage element,
  `fullscreenchange` listener flips the icon, ESC exits natively.
- **Remote Resolution** (`rc:resolution` control-DC message): tells
  the agent to capture/encode at a specific size. Modes:
  `original` (native monitor), `fit` (match local stage ×
  `devicePixelRatio`, re-emitted on resize via `ResizeObserver`
  debounced 250 ms), `custom` (preset chips + free-form W×H).
  Persisted per-agent (keyed on `agentId`) so "Fit to local
  1920×1080" on my laptop doesn't bleed to my 4K desktop.

**No SDP renegotiation needed for resolution changes** —
H.264 / H.265 / AV1 all carry resolution in the SPS/VPS NALU;
browsers handle mid-stream size changes on the existing RTP track.
The agent's existing `encoder_dims != Some((w, h))` rebuild branch
already handles dim changes (docking, DPI toggle); `rc:resolution`
just writes a new target into the shared `TargetResolution` atomic
and `apply_target_resolution` downscales the captured frame via
`downscale_bgra_box` (CPU box filter, ~30 ms on 4K→1080p) before
encode. GPU `VideoProcessorMFT` path stays in the deferred 1C.3
bucket.

### 18.7 Diagnostics (0.1.34)

Added in response to the field-reported 7-8 fps case on a hybrid
RTX 5090 + Intel UHD 630 box:

- Media-pump heartbeat log now reports `avg_capture_ms` /
  `avg_encode_ms` per 30-frame window (reset per window so
  transient stalls don't smear over a long session).
- WGC `SharedSlot` tracks `arrived_total` + `dropped_stale` and
  logs `wgc: capture cadence arrived=N drops=M drop_ratio_pct=P`
  every ~120 arrivals. Low `arrived_total` means WGC itself is
  starving (iGPU scheduling); high `drop_ratio_pct` means
  consumer (encode) can't keep up.

Root cause of that specific 7-fps case: NVENC Blackwell
`ActivateObject` returns `0x8000FFFF` for H.264/HEVC/AV1 on RTX
5090 → cascade lands on Intel UHD 630 HEVC MFT → can't sustain
4K@30. Workaround: operator picks `Remote Resolution = Fit` (or
`Custom: 1920×1080`), agent CPU-downscales, UHD 630 HEVC holds 30-60
fps comfortably at that size. Proper fix deferred to Tier 1C.3
(GPU-side scale via `VideoProcessorMFT`).

## 19. Resilience cycle (0.1.50 → 0.1.54)

Multi-release hardening of the agent's lifecycle: persistent
diagnostics, OS-supervisor parity across Win/Linux/macOS, integrity-
verified updates, automatic rollback, and turn-key install. Five
patch releases shipped 2026-04-29 in a single push. Total of ~3700
LOC added, ~150 unit tests; 0 deferred to next cycle from the
P0 cut.

### 19.1 Failure-resilience P0 (0.1.50)

The five P0 phases that made the agent stop dying silently.

**Persistent file logging + panic hook** (`agents/roomler-agent/src/
logging.rs`): daily-rolling appender via `tracing-appender` at the
platform data-local dir (`%LOCALAPPDATA%\roomler\roomler-agent\
data\logs\` on Win; `~/.local/share/roomler-agent/logs/` on Linux;
`~/Library/Application Support/live.roomler.roomler-agent/logs/`
on macOS). 14-day retention via prune-on-startup. `WorkerGuard`
held in a `OnceLock<WorkerGuard>` so the writer thread survives
process lifetime. Process-wide `std::panic::set_hook` writes a
sync `panic-<pid>-<unix>.log` with `Backtrace::force_capture()`
output BEFORE delegating to the previous hook — the sync write
is the belt-and-braces against the non-blocking appender's worker
not draining the queue before the OS reaps a panicking process.

**Windows Scheduled Task XML rewrite** (`agents/roomler-agent/src/
service.rs::render_task_xml`): replaced `schtasks /Create /SC
ONLOGON ...` with `schtasks /Create /XML <utf-16-le-bom-tempfile>`.
Schema 1.2 (broadest universally-supported version, Win 7+).
Settings that were previously missing or wrong:
- `<RestartOnFailure><Interval>PT1M</Interval><Count>10</Count></RestartOnFailure>`
- `<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>`
- `<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>` (default
  is `true` — silently kills agent on laptop unplug)
- `<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>`
- `<StartWhenAvailable>true</StartWhenAvailable>`
- Belt-and-braces `<EventTrigger>` on EventID 12 (Microsoft-Windows-
  Kernel-General "operating system started") for kiosk auto-logon
  hosts where the LogonTrigger may fire before user session is
  fully ready

Brings Windows to parity with systemd `Restart=on-failure` (already
in `packaging/linux/roomler-agent.service`) and macOS launchd
`KeepAlive` (already in `packaging/macos/com.roomler.agent.plist`).

**Single-instance lock** (`agents/roomler-agent/src/instance_lock.rs`):
prevents an interactive `roomler-agent run` from racing the
Scheduled-Task / systemd-launched copy in the same user session.
Win: `CreateMutexW` named `Local\RoomlerAgent-<sha-prefix12-of-
config-path>` (`Local\` namespace = per-session scope; SHA disambig
covers two enrolments on the same machine for the same user). Unix:
`flock(LOCK_EX | LOCK_NB)` on `$XDG_RUNTIME_DIR/roomler-agent-<id>.
lock` (or `~/.cache/...` fallback) with PID written for diagnostics.
Both kernel-released on process death, no stale-lock cleanup needed
after `kill -9`. Only `run` gates on the lock — `enroll`,
`service install/uninstall`, `caps`, `displays`, `encoder-smoke`,
`self-update` stay runnable alongside a live agent.

**Internal liveness watchdog** (`agents/roomler-agent/src/watchdog.
rs`): process-singleton via `OnceLock<Arc<Watchdog>>`; pumps tick
via global `watchdog::tick("name")` free helpers (no parameter
threading). Per-pump thresholds — signaling: 90s (keepalive cadence
25s × 3 grace); encoder + capture: 30s (gated on session-active so
they ignore quiet idle periods between sessions). Async `run()`
loop wakes every 5s, scans, force-exits via `std::process::exit(2)`
(sentinel code distinct from 0 + 1) on stall — relies on the OS
supervisor to relaunch a healthy copy. Suspend handling: if a loop
iteration takes more than `SCAN_INTERVAL + 60s`, treat as wall-clock
jump (laptop close-lid → resume) and reset all pump heartbeats
instead of declaring a stall. **Watchdog-of-watchdog** runs on a
dedicated `std::thread` (the only `std::thread` in the codebase),
wakes every 30s, force-exits if the async watchdog hasn't bumped
its `AtomicU64` heartbeat in 60s — catches a fully-deadlocked
tokio runtime.

**Token revocation grace** (`agents/roomler-agent/src/signaling.rs`):
replaced the `AuthRejected → hard exit` branch with a backoff ladder
(`auth_backoff_for`): 30s → 60s → 5min → 1h capped. Server-side
JWT cache flushes during a deploy used to permanently break every
agent in the field; now they back off and rejoin within seconds.
After 3 consecutive 401s, raises `<config-dir>/needs-attention.txt`
sentinel via `notify::raise_attention` describing the situation +
recommending `roomler-agent re-enroll --token <jwt>` (new CLI
that preserves `machine_id` + `machine_name` from the existing
config). Sentinel auto-cleared on auth recovery.

### 19.2 Update-path hardening (0.1.51)

**Configurable update cadence**: `update_check_interval_h: Option<u32>`
config field + `ROOMLER_AGENT_UPDATE_INTERVAL_H` env var override.
Pure resolver `resolve_check_interval_with(env_value, cfg_value)`
extracted so tests don't race on process env. Defaults to the
existing 24 h built-in.

**Post-install watcher** (`agents/roomler-agent/src/post_install.rs`):
new hidden CLI subcommand `roomler-agent post-install-watch
--installer-pid <pid> --installer-path <path> --expected-version
<tag>`. Spawned by `updater::spawn_installer_with_watch` as a
sibling of msiexec / dpkg / installer(8) just before the parent
agent exits to make room for the installer. Watcher polls the
installer PID (Win `OpenProcess` + `WaitForSingleObject`; Unix
`kill(pid, 0)` loop), captures the exit code, sleeps 2s for FS
settle, runs the new binary's `--version`, writes a typed JSON
outcome to `<log_dir>/last-install.json` with status
`InProgress` / `SucceededVerified` / `SucceededUnverified` /
`InstallerFailed` / `Timeout`. Operators + future agent startups
read the file to surface what actually happened to the upgrade.

**AgentConfig crash-tracking fields** (all `#[serde(default)]` for
back-compat — pre-0.1.51 configs continue to load, locked by
`old_config_without_new_fields_loads_with_defaults`): `last_known_
good_version: Option<String>`; `crash_count: u32`; `last_crash_unix:
u64`; `rollback_attempted: bool`; `last_run_unhealthy: bool`.

**Crash-loop detection**: at `run_cmd` startup, if
`last_run_unhealthy=true` (previous run started but never reached
`CLEAN_RUN_THRESHOLD_SECS=300` and didn't exit gracefully via
Ctrl-C), `record_crash_at` bumps the in-window counter
(`CRASH_WINDOW_SECS=600`). After 5 min of healthy signaling, a
background tokio task promotes the running version to
`last_known_good_version` and resets the counter via
`record_clean_run_at`. Ctrl-C handler also clears the unhealthy
flag (`mark_clean_shutdown`). When `should_rollback` returns true
(3 crashes within 10 min, target != current, !rollback_attempted),
the agent raises an operator-attention sentinel.

### 19.3 SHA256 verification + automatic rollback (0.1.52)

**SHA256 asset verification** (`updater::verify_sha256`): GitHub
Releases API exposes a `digest` field per asset of the form
`"sha256:<hex>"` (added late 2024). Forwarded by the proxy via
`AgentReleaseAsset.digest`. Agent computes SHA256 of downloaded
bytes via `sha2`, compares case-insensitive against the digest;
mismatched downloads NEVER touch disk. Refuses unsupported
algorithms loud (`sha512:...` etc) so a future GitHub format
change fails-loud rather than silently disabling verification.
Falls through to the existing `MIN_INSTALLER_BYTES` size floor
when digest is absent (pre-2024 releases or proxy that doesn't
yet forward the field).

**`updater::pin_version(tag)`**: fetches a specific release from
`https://api.github.com/repos/.../releases/tags/<tag>` directly
(bypasses the roomler-ai proxy because pinning is rare per-agent
crash-loop recovery, not a fleet-wide poll). Returns the same
`CheckOutcome::UpdateReady` shape as the regular update path so
the rest of the install flow composes.

**Automatic rollback execution**: when `should_rollback` fires AND
`last_known_good_version` is set AND it's different from current:
mark `rollback_attempted=true` FIRST (so a crash during the
rollback fetch can't loop us into another rollback) → save → call
`pin_version(format!("agent-v{target}"))` → on `UpdateReady`,
spawn the installer with the post-install watcher, exit so the
installer can overwrite the binary. Failure modes (Skipped,
UpToDate, spawn error) raise the operator-attention sentinel
with a remediation link to the GitHub releases page so the
operator can downgrade manually.

### 19.4 Schema 1.3+ regression fix (0.1.53)

0.1.50's `service install` shipped `<DisallowStartOnRemoteAppSession>`
and `<UseUnifiedSchedulingEngine>` inside a `<Task version="1.2">`
document — both are Schema 1.3+ Settings.* children. schtasks /XML
on Win10/11 correctly rejected the document with
`(39,7):DisallowStartOnRemoteAppSession: ERROR: The task XML
contains an unexpected node`. Field impact: anyone who tried
`roomler-agent service install` after 0.1.50–0.1.52 saw the error
and kept their pre-0.1.50 ONLOGON task. Their *binary* had all the
in-process resilience features but their *Scheduled Task* still had
the bad battery defaults.

Fix: removed both elements (neither was load-bearing — the
resilience-critical settings are all Schema 1.2 native). Locked by
`xml_template_excludes_schema_1_3_only_elements` regression test
so a future "let me bump these back in" diff fails CI.

### 19.5 MSI auto-registers Scheduled Task (0.1.54)

WiX custom actions in `agents/roomler-agent/wix/main.wxs`:

- `RegisterAutostart`: `FileKey='roomler_agent_exe'
  ExeCommand='service install' Execute='deferred' Impersonate='yes'
  Return='ignore'`. Sequenced `After='InstallFiles'` with condition
  `NOT (REMOVE="ALL")` so it runs on fresh install + repair +
  MajorUpgrade but skips during full uninstall.
- `UnregisterAutostart`: same shape with `service uninstall`.
  Sequenced `Before='RemoveFiles'` (the action shells out to the
  EXE so it must still be on disk) with condition `REMOVE="ALL"`
  so it only fires during full uninstall, not modify/repair.

perUser MSI runs in the user's token (no UAC, no SYSTEM
impersonation complications). `Return="ignore"` so an existing-
task ACL conflict (the rare Win11 quirk that bit the field on
2026-04-29 — see §19.7) doesn't sink the install.

Closes the UX gap that bit operators upgrading from 0.1.49 → 0.1.5x:
the new task XML shipped inside `service::install()` but the MSI
never ran it. From 0.1.54 onwards every install + upgrade refreshes
the task definition automatically.

### 19.6 CI hardening (during 0.1.54 cycle)

`continue-on-error: true` on the three `Cache cargo` steps in
`.github/workflows/release-agent.yml`. agent-v0.1.53 attempt 1
failed despite every build / smoke / artifact-upload step
succeeding — the post-job tar/zstd cache write returned non-zero,
which marked the whole job as failure and caused
`Publish GitHub Release` to skip. Attempt 2 (manual rerun) was
green. Cache is an optimisation, not a correctness gate.

### 19.7 Field gotcha: Win11 ACL-locked Scheduled Tasks

Discovered while validating 0.1.52 → 0.1.53: existing tasks
created via `schtasks /Create /SC ONLOGON` (the pre-0.1.50 path)
can develop a tightened ACL that denies even the owner Modify and
Delete rights without elevation. Symptom on goran-xmg-neo16:
`Unregister-ScheduledTask` returns `HRESULT 0x80070005`
(E_ACCESSDENIED), and `schtasks /Create /XML /F` fails with
`Access is denied` even though the existing task's `Author` field
matches the current user. UAC token-filtering on Win11 is the
likely root cause (admin users run with a filtered non-admin token
by default; modifying certain scheduled-task properties requires
the unfiltered token).

**Recovery** (one-time, per machine that hits this):
```powershell
# Elevated PowerShell:
schtasks /Delete /TN RoomlerAgent /F

# Normal PowerShell (post-0.1.54):
& "$env:LOCALAPPDATA\Programs\roomler-agent\roomler-agent.exe" service install
```

After this, the freshly-created task has a normal ACL and future
upgrades can self-manage. From 0.1.54 onwards new installs never
hit this — only pre-existing locked tasks.

### 19.8 Phase 7 / 8 / Effort 2 cycle — shipped 0.1.55 → 0.1.58

The "next session" plan from §19.8's prior revision shipped on
2026-04-30 across four patch releases. Three closed cleanly; M3 + M5
of Effort 2 are queued for follow-up.

#### 19.8.1 agent-v0.1.55 — enrollment http→https normalization

Field bug: `roomler-agent enroll --server http://roomler.ai ...` failed
with `enrollment rejected (status 405 Method Not Allowed)`. Cause: the
production ingress 301-redirects plaintext to TLS; reqwest follows the
redirect but converts POST→GET (RFC 7231 historical behaviour for
301/302), and a GET on `/api/agent/enroll` is unmatched.

`enrollment::normalize_server_url` upgrades `http://` → `https://`
upfront with a warn log and stores the normalized form in the agent
config so `derive_ws_url()` also yields wss:// for the long-lived
signaling connection. Bonus security win: enrollment tokens never
leave the wire in cleartext.

#### 19.8.2 agent-v0.1.56 — Phase 7 heartbeat telemetry

Closes the "agent shows online forever after silent disconnect" gap.
Agents emit `ClientMsg::AgentHeartbeat { rss_mb, cpu_pct,
active_sessions }` every 30 s on the existing `/ws` connection; the
wire format already existed but neither side was using it.

Server (`crates/api/src/ws/remote_control.rs`) checks for the
heartbeat variant in the read loop and calls the new
`agents.touch_heartbeat(agent_id)` DAO method, which writes
`last_seen_at = now()`. Best-effort: a Mongo lag warns rather than
dropping the WS. With this in place, "agent online" can be defined as
`last_seen_at > now − 90 s` (3× cadence tolerance for one missed
tick).

Agent (`agents/roomler-agent/src/signaling.rs`) adds a 30 s
`tokio::time::interval` arm to the connect_once `select!`. v1 sends
`rss_mb=0`, `cpu_pct=0.0` (process-self metrics deferred to a follow-
up that adds the `sysinfo` crate); `active_sessions = peers.len()`
straight off the per-connection peer map.

Backend deployed via mars rebuild → ArgoCD GitOps in the same cycle.

#### 19.8.3 agent-v0.1.57 — Phase 8 pre-flight checks

New `preflight` module runs three parallel probes right after config
load, before the signaling loop kicks in:

  - DNS via `tokio::net::lookup_host`. Hint: "check /etc/hosts, the
    system DNS resolver, or whether VPN is required".
  - TCP via `tokio::net::TcpStream::connect`. Hint: "check firewall
    outbound rules, corporate proxy / captive portal".
  - Clock skew via HEAD `<server>/health` + RFC2822 Date: parse.
    Warns past ±60 s. Hint: "JWT validation will fail past ±60 s;
    sync time (w32time / chronyd / ntpd)".

Non-blocking — each finding is a `warn!` with `hint=` field. Total
budget ~5 s wall (parallel join); 5 s per probe individually. Bad
URL surfaces a `BadServerUrl` finding rather than silently no-oping.

#### 19.8.4 agent-v0.1.58 — Effort 2 M1+M2 (Windows Service mode)

Optional opt-in alternative to the Scheduled Task auto-start, for
fleet / unattended deployments. Two milestones landed; M3 + M4 + M5
are deferred (see §19.8.5).

**M1 — service host skeleton** (`agents/roomler-agent/src/win_service/
mod.rs`):

  - `install(exe_path)` registers `RoomlerAgentService` with the SCM
    (LocalSystem, AutoStart, OWN_PROCESS), launches via the agent's
    own hidden `service-run` subcommand on start.
  - `uninstall()` stops (best-effort 5 s) + deletes; tolerates
    ERROR_SERVICE_DOES_NOT_EXIST (1060) for idempotent rollback.
  - `status()` returns `InstalledStatus` enum.
  - `run_in_dispatcher()` is the SCM entry point. Hands the OS thread
    to `windows_service::service_dispatcher::start`.
  - SCM control handler accepts STOP + PRESHUTDOWN + SESSION_CHANGE.

CLI: `service install --as-service` / `service uninstall --as-service`
/ `service status --as-service`. Hidden top-level `service-run`
invoked by the SCM's StartService.

Crate: `windows-service = "0.7"` for the SCM lifecycle.

**M2 — session-aware worker supervisor** (`win_service/supervisor.rs`):

  - `active_console_session_id()` wraps `WTSGetActiveConsoleSessionId`;
    None for the 0xFFFFFFFF "no active session" sentinel.
  - `query_user_token(session_id)` wraps `WTSQueryUserToken`; tolerates
    ERROR_NO_TOKEN and returns None for the M3 SYSTEM-context case.
  - `unsafe spawn_in_session(token, exe, args)` calls
    `CreateEnvironmentBlock` + `CreateProcessAsUserW` with
    CREATE_UNICODE_ENVIRONMENT + CREATE_NEW_CONSOLE; attaches to
    `winsta0\default` desktop. Returns `OwnedProcess`.
  - `OwnedHandle` / `OwnedProcess` / `EnvBlock` RAII wrappers — no
    handle leaks even on `?` early returns.
  - `decide_spawn(active, current_worker_session) →
    {SpawnIn|KeepCurrent|Idle}` — pure state machine, exhaustively
    unit-tested without FFI.
  - `next_backoff(consecutive_failures)` — 2 s × 2^(n−1) capped at
    60 s, saturating against runaway counters; mirrors the
    Scheduled Task `RestartOnFailure` PT1M cap.
  - `run(exe, args, rx)` is the supervisor main loop on a dedicated
    OS thread. SCM session-change notifications swap the worker to
    the new active console user; worker crash respawns under backoff.

Crate: `windows-sys = "0.59"` for the raw Win32 FFI (
Win32_System_RemoteDesktop, _Threading, _Environment,
_StationsAndDesktops). Going through windows-sys (not the higher-
level `windows` crate) keeps service-mode independent of the
feature-gated MF / WGC compilation paths.

**M4 (MSI integration) deferred-by-design**: the agent MSI is
`InstallScope='perUser'` (no UAC), but `CreateService` requires admin
elevation. Auto-registering `RoomlerAgentService` from a perUser MSI
is impossible. Operators install the MSI normally, then run
`roomler-agent service install --as-service` from an elevated
PowerShell. Future per-machine MSI flavour can revisit.

### 19.8.5 Pending — picked up in next session

- **M3 (pre-logon SYSTEM-context capture)**: when no console session
  has a logged-in user yet, the service itself runs capture + input
  inline under SYSTEM rather than spawning a worker. Tightened
  capability surface: capture + input only, no clipboard / file-
  transfer / audio. Hand-off when a user logs in: tear down the
  SYSTEM-context capture, spawn a user-context worker via the M2
  supervisor path. Risky — needs hands-on Win11 testing of WGC
  capture from session 0 against the lock screen.
- **M5 (verification on a real Win11 install)**: test M1 + M2 on
  goran-xmg-neo16: `service install --as-service` from elevated
  PowerShell, observe the `roomler-svc-supervisor` thread spawning
  a worker in the active session, lock screen + verify the worker
  is gone, unlock + verify it respawns.
- **Per-machine MSI flavour** (the M4-fix-up): a separate WiX file
  `wix/main-permachine.wxs` with `InstallScope='perMachine'`,
  RegisterService deferred-non-impersonated custom action. Builds a
  separate `.msi` artifact in the release workflow alongside the
  perUser one.
