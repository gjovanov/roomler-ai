# Real-Time

Roomler2 uses WebSocket for real-time features: presence updates, typing indicators, and server-pushed events.

## Connection Flow

```
Browser                                     Axum Server
  │                                              │
  │  GET /ws?token=<JWT>                         │
  ├─────────────────────────────────────────────►│
  │                                              │
  │  (Server verifies JWT, extracts user_id)     │
  │                                              │
  │  101 Switching Protocols                     │
  │◄─────────────────────────────────────────────┤
  │                                              │
  │  { "type": "connected",                      │
  │    "user_id": "6..." }                       │
  │◄─────────────────────────────────────────────┤
  │                                              │
  │  ─── bidirectional messages ───              │
  │                                              │
```

1. Client opens WebSocket to `/ws?token=<JWT>` (JWT is passed as query parameter since WS handshake cannot use cookies/headers)
2. Server verifies the JWT before accepting the upgrade
3. On success, connection is registered in `WsStorage` under the user's ID
4. Server sends a `connected` confirmation message
5. Bidirectional message exchange begins

## Message Types

### Server → Client

| Type | Payload | Description |
|------|---------|-------------|
| `connected` | `{ user_id }` | Connection established confirmation |
| `pong` | `{}` | Response to client ping |
| `typing:start` | `{ channel_id, user_id }` | User started typing in channel |
| `typing:stop` | `{ channel_id, user_id }` | User stopped typing in channel |
| `presence:update` | `{ user_id, presence }` | User presence changed |

### Client → Server

| Type | Payload | Description |
|------|---------|-------------|
| `ping` | `{}` | Application-level keepalive |
| `typing:start` | `{ channel_id }` | Notify channel members of typing |
| `typing:stop` | `{ channel_id }` | Notify channel members typing stopped |
| `presence:update` | `{ presence }` | Update own presence status |

All messages are JSON:

```json
{
  "type": "typing:start",
  "data": {
    "channel_id": "6..."
  }
}
```

## WsStorage

`WsStorage` tracks all active WebSocket connections with dual indexing:

- **`connections`**: `DashMap<ObjectId, Vec<WsSender>>` -- user-level (for user-targeted broadcasts)
- **`connection_map`**: `DashMap<String, (ObjectId, WsSender)>` -- connection-level (for connection-targeted sends)

Each user can have **multiple connections** (multiple browser tabs, devices). Each connection gets a unique `connection_id` (UUID) generated server-side when the WebSocket connects.

```rust
pub struct WsStorage {
    connections: DashMap<ObjectId, Vec<WsSender>>,
    connection_map: DashMap<String, (ObjectId, WsSender)>,
}
```

Key operations:
- `add(user_id, connection_id, sender)` -- register a new connection (both indexes)
- `remove(user_id, connection_id, sender)` -- unregister using Arc pointer equality + connection_id
- `get_senders(user_id)` -- get all senders for a user (for user-level broadcasts)
- `get_sender_by_connection(connection_id)` -- get sender for a specific connection (for media signaling responses)
- `all_user_ids()` -- list all connected users
- `connection_count()` -- total active connections across all users

## Dispatcher

The dispatcher sends messages at three levels:

- **`send_to_user(ws_storage, user_id, message)`** -- send to ALL connections of a specific user
- **`send_to_connection(ws_storage, connection_id, message)`** -- send to ONE specific connection (used for media signaling responses like `router_capabilities`, `transport_created`, `produce_result`, `consumer_created`)
- **`broadcast(ws_storage, user_ids, message)`** -- send to all connections of multiple users

### Broadcast Scoping

| Event | Recipients | Targeting |
|-------|-----------|-----------|
| `typing:start` / `typing:stop` | All members of the channel **except** the sender | User-level |
| `presence:update` | All connected users | User-level |
| `pong` | Only the sender | User-level |
| `message:create` | All members of the channel **except** the sender | User-level |
| `media:router_capabilities` | Only the requesting connection | Connection-level |
| `media:transport_created` | Only the requesting connection | Connection-level |
| `media:produce_result` | Only the producing connection | Connection-level |
| `media:consumer_created` | Only the consuming connection | Connection-level |
| `media:new_producer` | All participants except the producer | User-level |
| `media:peer_left` | All remaining participants | User-level |
| `media:producer_closed` | All participants except the producer | User-level |

For typing indicators, the server looks up channel member IDs and broadcasts to all members except the typing user. For presence, the update goes to all connected users. For message creation, the sender is excluded from broadcast to prevent duplicate display (the sender already has the message from the HTTP response).

## Presence

Users have one of five presence states:

| State | Description |
|-------|-------------|
| `online` | Actively connected and interacting |
| `idle` | Connected but inactive |
| `dnd` | Do not disturb (suppresses notifications) |
| `offline` | Not connected (default) |
| `invisible` | Connected but appears offline to others |

Presence is updated via the WebSocket `presence:update` message and broadcast to all connected users.

## Protocol-Level Ping/Pong

In addition to application-level `ping`/`pong` messages, the server handles WebSocket protocol-level `Ping` frames by responding with `Pong` frames automatically. This keeps the connection alive at the transport layer.

## mediasoup SFU Integration

Roomler2 uses mediasoup as an SFU (Selective Forwarding Unit) for WebRTC video/audio conferencing.

### Configuration

```
ROOMLER__MEDIASOUP__NUM_WORKERS=2        # mediasoup worker processes
ROOMLER__MEDIASOUP__LISTEN_IP=0.0.0.0    # bind address
ROOMLER__MEDIASOUP__ANNOUNCED_IP=1.2.3.4 # public IP (for NAT traversal)
ROOMLER__MEDIASOUP__RTC_MIN_PORT=40000   # UDP port range start
ROOMLER__MEDIASOUP__RTC_MAX_PORT=49999   # UDP port range end
```

### Architecture

```
WorkerPool (round-robin N mediasoup workers)
  └── RoomManager
        ├── rooms: DashMap<ObjectId, MediaRoom>
        │     └── MediaRoom
        │           ├── router: Router
        │           └── participants: DashMap<String, ParticipantMedia>
        │                 └── ParticipantMedia
        │                       ├── user_id: ObjectId
        │                       ├── send_transport: WebRtcTransport
        │                       ├── recv_transport: WebRtcTransport
        │                       ├── producers: Vec<Producer>
        │                       └── consumers: Vec<Consumer>
        └── connection_rooms: DashMap<String, ObjectId>
```

### Connection ID Architecture

Participants are keyed by **connection_id** (UUID per WebSocket connection), not by user_id. This enables:

- **Multi-tab/device support**: The same user can join from multiple tabs without state overwriting
- **Independent state**: Each connection has its own transports, producers, and consumers
- **Proper cleanup**: Closing one connection doesn't destroy another's media state

The server generates a unique `connection_id` for each WebSocket connection at connect time and uses it for all room_manager operations.

### Media Signaling Flow

```
Browser                             Server (Axum + mediasoup)
  │                                       │
  │  HTTP POST /conference/{id}/start     │  (creates Router)
  │  HTTP POST /conference/{id}/join      │  (DB participant record)
  │                                       │
  │  WS: media:join {conference_id}       │
  ├──────────────────────────────────────►│
  │                                       │  create_transports(conf, user, connection_id)
  │  WS: media:router_capabilities        │  (connection-targeted)
  │◄──────────────────────────────────────┤
  │  WS: media:transport_created          │  (connection-targeted)
  │◄──────────────────────────────────────┤
  │  WS: media:new_producer (existing)    │  (for each existing producer)
  │◄──────────────────────────────────────┤
  │                                       │
  │  WS: media:connect_transport          │  (DTLS handshake)
  ├──────────────────────────────────────►│
  │                                       │
  │  WS: media:produce {kind, rtp_params} │
  ├──────────────────────────────────────►│
  │  WS: media:produce_result {id}        │  (connection-targeted)
  │◄──────────────────────────────────────┤
  │                                       │  broadcast media:new_producer to peers
  │                                       │
  │  WS: media:consume {producer_id}      │
  ├──────────────────────────────────────►│
  │  WS: media:consumer_created           │  (connection-targeted)
  │◄──────────────────────────────────────┤
  │                                       │
  │  WS: media:leave                      │
  ├──────────────────────────────────────►│  close_participant(conf, connection_id)
  │                                       │  broadcast media:peer_left to peers
```

### Key Design Decisions

1. **Connection-targeted vs user-targeted sends**: Media signaling responses (capabilities, transports, produce results) are sent to the specific connection via `send_to_connection()`. Peer notifications (new_producer, peer_left) are broadcast to all participants via user_ids.

2. **Sender exclusion**: Message broadcasts exclude the sender's user_id to prevent duplicates. The frontend also has dedup (checking by message ID) as a safety net.

3. **HTTP leave cleanup**: The HTTP leave endpoint uses `close_participant_by_user()` which removes ALL connections for that user (since it doesn't know the connection_id). The WS leave and disconnect path uses `close_participant()` with the specific connection_id.

4. **Race condition mitigation**: The frontend registers `media:new_producer` handlers BEFORE sending `media:join`, and buffers any producer messages that arrive before transports are ready.

TURN server (Coturn) is configured for NAT traversal via `ROOMLER__TURN__URL`, `ROOMLER__TURN__USERNAME`, `ROOMLER__TURN__PASSWORD`.
