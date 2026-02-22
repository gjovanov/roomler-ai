# Use Cases

## Permission System

Roomler2 uses a **u64 bitfield** for permissions. Each bit represents one permission flag. Roles are assigned to tenant members, and the effective permission is the union of all assigned role permissions.

### Permission Flags (24 bits)

| Bit | Flag | Description |
|-----|------|-------------|
| 0 | `VIEW_ROOMS` | See rooms |
| 1 | `MANAGE_ROOMS` | Create, edit, delete rooms |
| 2 | `MANAGE_ROLES` | Create, edit, delete roles |
| 3 | `MANAGE_TENANT` | Edit tenant settings |
| 4 | `KICK_MEMBERS` | Remove members |
| 5 | `BAN_MEMBERS` | Ban members |
| 6 | `INVITE_MEMBERS` | Create invites |
| 7 | `SEND_MESSAGES` | Send messages |
| 8 | `SEND_THREADS` | Reply in threads |
| 9 | `EMBED_LINKS` | URLs auto-preview |
| 10 | `ATTACH_FILES` | Upload file attachments |
| 11 | `READ_HISTORY` | Read message history |
| 12 | `MENTION_EVERYONE` | Use @everyone and @here |
| 13 | `MANAGE_MESSAGES` | Delete/pin others' messages |
| 14 | `ADD_REACTIONS` | Add emoji reactions |
| 15 | `CONNECT_VOICE` | Join voice rooms |
| 16 | `SPEAK` | Speak in voice rooms |
| 17 | `STREAM_VIDEO` | Share video/screen |
| 18 | `MUTE_MEMBERS` | Server-mute others |
| 19 | `DEAFEN_MEMBERS` | Server-deafen others |
| 20 | `MOVE_MEMBERS` | Move members between voice rooms |
| 21 | `MANAGE_MEETINGS` | Create, start, end conferences |
| 22 | `MANAGE_DOCUMENTS` | Manage files and documents |
| 23 | `ADMINISTRATOR` | Bypasses all permission checks |

### Default Role Permissions

| Role | Flags Included |
|------|---------------|
| **DEFAULT_MEMBER** | VIEW_ROOMS, SEND_MESSAGES, SEND_THREADS, EMBED_LINKS, ATTACH_FILES, READ_HISTORY, ADD_REACTIONS, CONNECT_VOICE, SPEAK, STREAM_VIDEO |
| **DEFAULT_ADMIN** | DEFAULT_MEMBER + MANAGE_ROOMS, MANAGE_ROLES, KICK_MEMBERS, BAN_MEMBERS, INVITE_MEMBERS, MENTION_EVERYONE, MANAGE_MESSAGES, MUTE_MEMBERS, DEAFEN_MEMBERS, MOVE_MEMBERS, MANAGE_MEETINGS, MANAGE_DOCUMENTS |
| **ALL (Owner)** | All 24 bits set (includes ADMINISTRATOR) |

### Permission Check Logic

```
has(permissions, flag) = (permissions & ADMINISTRATOR != 0) || (permissions & flag == flag)
```

The `ADMINISTRATOR` flag bypasses all other checks.

### Room Permission Overwrites

Rooms can override base permissions per-role or per-user:

```
effective = (base_permissions & ~deny) | allow
```

Each `PermissionOverwrite` specifies a `target_id` (role or user), an `allow` mask, and a `deny` mask.

## Authentication Flow

```
┌──────────┐   POST /api/auth/register   ┌──────────┐
│  Browser  ├───────────────────────────►│  Axum    │
│           │   { email, username,       │  API     │
│           │     display_name, password,│          │
│           │     tenant_name?,          │          │
│           │     tenant_slug? }         │          │
│           │                            │          │
│           │◄───────────────────────────┤          │
│           │   Set-Cookie: access_token │          │
│           │   { access_token,          │          │
│           │     refresh_token,         │          │
│           │     expires_in, user }     │          │
└──────────┘                             └──────────┘
```

1. **Register** -- user provides email, username, display_name, password. Optionally creates a tenant.
2. **Login** -- by username or email + password. Argon2 hash verification.
3. **Token delivery** -- JWT access token set as httpOnly cookie. Refresh token returned in body.
4. **Protected requests** -- access token read from cookie or `Authorization: Bearer` header.
5. **Token refresh** -- POST refresh_token to get new access + refresh tokens.

## Room Lifecycle

```
Create Room
     |
     v
  Room exists -------> RoomMember created
     |                           |
     v                           v
  Send messages             Read messages
     |                           |
     |-- Start thread            |-- Unread tracking
     |-- Add reactions           |-- Mention tracking
     |-- Pin messages            |-- Notification prefs
     |-- Upload files            |-- Mute room
     |-- Start call              |
     |                           v
     |                      Leave Room
     |                           |
     v                           v
  Call lifecycle           RoomMember deleted
```

### Room Types

Rooms are polymorphic — a room's capabilities are determined by its fields:

| Configuration | Behavior |
|--------------|----------|
| `media_settings: None` | Text-only room (chat, threads, files) |
| `media_settings: Some(...)` | Voice/video capable (can start calls) |
| `conference_settings: Some(...)` | Scheduled/recurring call settings |
| `parent_id: Some(...)` | Child room (nested hierarchy) |
| `is_open: true` | Publicly joinable |
| `is_read_only: true` | Announcement-style (restricted posting) |

## Call Lifecycle

Calls happen inside media-enabled rooms (rooms with `media_settings` set).

```
Room (media_settings present)
     |
     v
  Start Call (POST /room/{id}/call/start)
     |
     v
  conference_status: InProgress
     |
     |-- WS broadcast: room:call_started to all room members
     |-- Participants join via WS media:join
     |-- Screen sharing
     |-- In-call chat messages
     |-- Recording starts
     |-- Transcription runs
     |
     v
  End Call (POST /room/{id}/call/end)
     |
     v
  conference_status: Ended
     |
     |-- WS broadcast: room:call_ended to all room members
     |-- Recordings processed (BackgroundTask)
     |-- Transcriptions generated (BackgroundTask)
```

## File Lifecycle

```
Upload File
     │
     ▼
  FileContext assigned (message/document/channel/conference/profile)
     │
     ├── Stored in MinIO (S3-compatible)
     ├── Virus scan (scan_status: pending → clean/malware)
     └── Version tracking (version chain via previous_version_id)
     │
     ▼
  AI Recognition (optional)
     │
     ▼
  Claude API extracts text/structure
     │
     ▼
  recognized_content populated
     │
     ├── raw_text
     ├── structured_data (JSON)
     ├── document_type
     └── confidence score
     │
     ▼
  Download / Cloud Sync
     │
     ├── Google Drive
     ├── OneDrive
     └── Dropbox
```

## Multi-Tenant Data Flow

All data is scoped to a tenant via `tenant_id`. The API URL structure enforces this:

```
/api/tenant/{tenant_id}/room/{room_id}/message
```

- Users can belong to multiple tenants (via `TenantMember`)
- Each tenant has its own roles, rooms, and files
- A user's permissions differ per tenant (based on assigned roles in that tenant)
- Cross-tenant data access is prevented at the DAO layer

## Invite Flow

```
Admin creates invite
     │
     ▼
  Invite { code, max_uses, expires_at, assign_role_ids }
     │
     ▼
  Share code/link
     │
     ▼
  Recipient accepts invite
     │
     ├── TenantMember created (or updated)
     ├── Roles assigned (assign_role_ids)
     └── use_count incremented
     │
     ▼
  Invite status:
     ├── active (still usable)
     ├── exhausted (use_count >= max_uses)
     ├── expired (past expires_at)
     └── revoked (manually disabled)
```

Invites can target a specific email or be open. They can optionally scope to a channel.
