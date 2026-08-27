# FR-12 — Onboarding tutorial ("Welcome tour")

**Issue:** [#788](https://github.com/gjovanov/roomler-ai/issues/788)
**Status:** P1 IMPLEMENTED (view + entry points + auto-open + progress); P2/P3 planned

## Goal

A first-time user landing in a fresh org has no guidance: the product's real
shape (devices + remote desktop + private network + tunnels + ACLs + rooms/
calls/chat) is discoverable only by clicking around. Ship an in-app **Tutorial**
with the README's visual language (`docs/assets/*.svg` hero illustrations +
"In detail" tables) that (a) opens for new users, (b) walks each capability
with do-it-now steps deep-linked into the live UI, and (c) stays **callable
anytime** by already-onboarded users.

## Visual language (reuse, don't reinvent)

`README.md` already defines it: one illustrated SVG per pillar —
`docs/assets/hero-mesh.svg`, `remote-desktop.svg`, `private-network.svg`,
`collaboration.svg` — plus concise capability tables. P1 reuses these four
verbatim (imported into the UI bundle); P3 adds same-style smaller
illustrations for the finer chapters (ACL, calls, chat).

## Design

### Surface: a dedicated Tutorial view, not a modal carousel

Route `tutorial` under the tenant shell (`/tenant/{tid}/tutorial`), rendered
like the other full views (h1 + content; the AdminPanel shell pattern). A
left chapter rail + scrollable chapter body:

| # | Chapter | Hero | Do-it-now steps (deep links) |
|---|---------|------|------------------------------|
| 0 | Get started | hero-mesh | create/rename org → enroll first device (opens the Enroll dialog on /devices) |
| 1 | Devices | hero-mesh | enroll (device vs tunnel client), grid tour (search/columns/tags/display name), update/pin |
| 2 | Remote desktop | remote-desktop | Connect from the grid, consent modes, clipboard + file transfer, codecs note |
| 3 | Private network | private-network | overlay addresses + MagicDNS name (copy from the grid), `roomler ssh <name>` |
| 4 | Tunnels | private-network | `roomler forward` / SOCKS5 one-liners (copyable, org-scoped), declared routes |
| 5 | ACL & policy | private-network | tunnel ACL default-deny, overlay ACL modes, subnet-route approval (/network/acl) |
| 6 | Rooms & chat | collaboration | create a room, invite someone (deep link /invites), mentions/files |
| 7 | Calls | collaboration | start a call from a room, screen share, the call badge |

Each chapter = hero SVG + one short paragraph (README tone) + a 2–4 item
checklist whose items are REAL router links (never screenshots of buttons) +
an "In detail" table borrowed/condensed from the README section.

### Entry points

1. **First login, auto-open**: after login, if `localStorage
   roomler-tour-seen:<userId>` is absent AND the active org looks fresh
   (0 devices — `deviceStore.total === 0` — and ≤1 room), route to the
   Tutorial instead of the dashboard, set the flag on first render.
   Dismissible instantly (the flag persists); never auto-opens twice.
2. **Callable anytime** (the explicit requirement): a `?` help icon in the
   app bar (next to notifications) → Tutorial; plus a "Tutorial" item in the
   user menu. Both visible to every member forever.
3. **Empty-state CTAs**: the devices grid's and rooms list's empty states
   link to their matching chapter (`/tutorial#devices`, `#rooms`).

### Progress

P1: per-chapter "done" checkmarks in `localStorage`
(`roomler:tour-progress:<userId>` — the useGridColumns storage conventions).
P3 (optional): mirror server-side on the user profile so progress follows
devices.

### Explicit non-goals

- No overlay/spotlight walkthrough of the live UI in P1 (P2 evaluates a
  dependency-free spotlight for the 3 core flows: enroll → connect → forward).
- No videos; SVG + text only (bundle size, translatability).
- No server round-trips in P1 — the tutorial is fully static + router links.

## Phases / status

| Phase | Content | Kill switch | Status |
|-------|---------|-------------|--------|
| P1 | Tutorial view (8 chapters, 4 reused SVGs), app-bar `?` + user-menu entries, first-login auto-open, localStorage progress | auto-open reads one localStorage flag; the route/nav entries are plain UI | **shipped** — PR #797 |
| P2 | Spotlight micro-tours for enroll/connect/forward on the live pages (dependency-free, ~100-line helper) | per-tour "skip"; entry only from the Tutorial | planned |
| P3 | New same-style SVGs for ACL/rooms/calls/chat chapters; server-side progress | none needed (additive) | planned |

## Acceptance criteria

- [ ] A brand-new user's first login lands on the Tutorial once, never twice;
      dismiss works instantly
- [ ] Every chapter renders its hero + steps; every step's deep link lands on
      the real page/dialog with the right tenant id
- [ ] The `?` app-bar icon and user-menu entry open the Tutorial for an
      onboarded user at any time
- [ ] Empty states on /devices and the rooms list link to their chapters
- [ ] `bun run test:unit` + `bun run build` green; no new runtime deps in P1

## Open decisions

1. Should the auto-open ALSO trigger for a user invited into an established
   org (org not fresh, user new)? Leaning yes-with-flag (same localStorage
   key — it is per-user).
2. Chapter for the desktop companion / setup wizard (installers) — or keep
   install coverage inside the Devices chapter's enroll step? Leaning inside.

## Out of scope

Marketing-site tours; per-plan gating (the tutorial shows everything and
labels plan-gated features as such); localization beyond the existing en.json.

## Field-verification log

- (pending P1)
