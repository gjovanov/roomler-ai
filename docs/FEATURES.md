# Features

Detailed descriptions of Roomler2 features.

## Theming

Roomler2 supports both light and dark themes, matching the original roomler.live color scheme.

- **Light theme** (default): Teal primary (#009688), grey background (#EEEEEE)
- **Dark theme**: Teal lighten-4 primary (#B2DFDB), dark background (#555555), dark surface (#333333)
- Toggle via sun/moon icon in the app bar
- Preference persisted in `localStorage` (`roomler-theme` key)
- Both themes share: secondary (#ef5350), accent (#424242), success (#69F0AE), warning (#FFC107), error (#DD2C00), info (#4DB6AC)

## @Mentions

Rich-text @mention autocomplete in the message editor using TipTap.

- Type `@` to trigger autocomplete
- Fuzzy search filters room members by display name
- Dropdown shows avatar + display name
- Special items: `@everyone` (all room members), `@here` (online members)
- Mentions are stored in the message's `mentions` field (user IDs, everyone/here flags)
- Mentioned users receive real-time notifications

### Implementation

- Frontend: `@tiptap/extension-mention` + tippy.js suggestion popup
- Backend: Message create handler extracts mentions, creates Notification documents, broadcasts via WebSocket

## Notifications

Real-time notification system for @mentions and other events.

- **Bell icon** in app bar with unread count badge
- **Dropdown panel** lists recent notifications
- Each notification shows: type icon, title, body, relative timestamp
- Click navigates to the source (message/room)
- "Mark all read" button
- Real-time delivery via WebSocket (`notification:new`, `notification:unread_count`)

### API Endpoints

- `GET /api/notification` — list notifications
- `GET /api/notification/unread` — list unread
- `GET /api/notification/unread-count` — count
- `PUT /api/notification/{id}/read` — mark read
- `POST /api/notification/read-all` — mark all read

## Room Roles & Permissions

Discord-like role system with a 24-bit permission bitfield.

### Default Roles (seeded on tenant creation)

| Role | Permissions |
|------|------------|
| Owner | All permissions (ADMINISTRATOR) |
| Admin | Default admin permissions |
| Moderator | Manage messages, kick, mute, manage meetings |
| Member | Send messages, react, connect voice, view channels |

### Permission Bits

Permissions are stored as a 24-bit integer. Key bits include:
- `ADMINISTRATOR` — all permissions
- `MANAGE_ROLES` — create/edit/delete roles
- `MANAGE_CHANNELS` — manage rooms
- `INVITE_MEMBERS` — create invites
- `KICK_MEMBERS`, `BAN_MEMBERS`
- `SEND_MESSAGES`, `MANAGE_MESSAGES`
- `CONNECT_VOICE`, `SPEAK`, `MUTE_MEMBERS`

### Role Management

- Role CRUD via API (create, update, delete custom roles)
- Cannot delete default/managed roles
- Assign/unassign roles to tenant members
- Role badges (colored chips) shown next to member names
- Role assignment dialog in member list

## User Profiles

User profiles with bio, avatar, and presence.

### Profile View (`/profile/:userId`)

- Large avatar with initials fallback
- Display name + @username
- Presence indicator (online/idle/dnd) with colored chip
- Bio section
- "Member since" date
- Edit button for own profile

### Profile Edit (`/profile/edit`)

- Display name (required)
- Bio (textarea, max 500 chars)
- Avatar URL
- Language selector (en-US, de-DE, fr-FR, es-ES, mk-MK, etc.)
- Timezone selector

## Explore Rooms

Discover and join open rooms within a tenant.

- Card grid layout (3 columns on desktop, 1 on mobile)
- Each card shows: room name, topic (or "No topic set"), member count, message count
- Search field with 300ms debounce
- Auto-loads on page mount
- Join button navigates directly to room chat
- Empty state with search icon

## Batch Invites

Invite multiple users at once with per-invite role assignment.

### Batch Invite Dialog

1. Add rows: email address + role selector
2. Shared expiration setting (default: 7 days)
3. "Send All" fires a single batch API call
4. Results shown per-invite (success/failure with error details)
5. Maximum 50 invites per batch

### API

- `POST /api/tenant/{tenant_id}/invite/batch` — accepts `{ invites: [...] }`
- Returns `{ results: [...], created: N, failed: N }`
- Each result includes the created invite or an error message
