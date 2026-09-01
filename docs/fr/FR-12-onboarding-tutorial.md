# FR-12 — Onboarding tutorial ("Welcome tour")

**Issue:** [#788](https://github.com/gjovanov/roomler-ai/issues/788)
**Status:** P1 + **P2 shipped**; P3 (extra artwork + server-side progress) planned

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
| P2 | Spotlight micro-tours on the live pages (dependency-free): module-scoped step machine + ONE overlay in `AppLayout` + a `?tour=` entry | per-tour "skip", and the only Tutorial entry is one button | **shipped** — PR #1117 |
| P3 | New same-style SVGs for ACL/rooms/calls/chat chapters; server-side progress | none needed (additive) | planned |

## P2 as built — two deviations from this spec, both deliberate

**"forward" is dropped, not deferred.** The phase named three tours —
enroll / connect / forward — but tunnel forwards are created from the CLI and
the desktop app. There is no web surface to spotlight, so a "forward" tour
could only have pointed at a page that does not do the thing. Recorded here
rather than left as a permanently-unstarted bullet.

**Only `enroll` gets a Tutorial button.** The `viewer` tour exists and works,
but its route needs an `agentId`; an entry in the Tutorial would land a reader
on a page with nothing to point at. It starts from `?tour=viewer` once you are
on a device, and the honest follow-up is a device-row action.

### Shape, and why it is not simpler

A module-scoped step machine, ONE overlay mounted in `AppLayout`, and a
`?tour=<id>` query the Tutorial navigates with. Neither half can live where you
would first put it: the state cannot live in the starter, because the Tutorial
view is unmounted by the time the tour runs; the overlay cannot live in each
page, because a page cannot mount itself into the page it is navigating to.

🔑 **Anchors are `data-tour` attributes, never CSS shapes.** A tour selecting
`.v-btn:nth-child(2)` breaks the first time someone adds a button — and breaks
SILENTLY, highlighting the wrong control instead of failing. The e2e suite lost
hours to exactly that class of locator in the same week
(`getByText('Chat')` matching four elements; `locator('input').first()`
resolving to a hidden nav field), which is why the guard test asserts every
anchor still EXISTS in the source: a renamed anchor otherwise just makes the
overlay flicker and skip a step, and nothing anywhere fails.

⚠️ Three behaviours worth keeping: the dimming is four plain divs rather than an
SVG mask (a mask can be clipped by a stacking context; the highlighted control
also stays interactive), a step whose anchor never renders advances after 1.5 s
rather than stranding the reader on a dimmed page, and the query param is
stripped on arrival so a refresh or a shared link cannot replay the tour.

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

- **2026-08-27, prod (`v20260827-cb368eefea67`) — P1 live.** `/tenant/{tid}/tutorial`
  serves 200 through the SPA fallback; the route's own lazy chunk
  (`TutorialView-*.js`) is referenced from the entry bundle, and all four
  hero SVGs return 200 at their built sizes (10.2–12.6 KB), i.e. the
  README artwork really ships rather than 404-ing behind a broken import.
  Chapter deep links are contract-tested against the route table, so a
  renamed route fails the build rather than dead-ending a reader.
  Remaining for closure: an operator read-through of the eight chapters
  (prose/accuracy), and one fresh-org account to watch the auto-open fire
  exactly once — both need a human, not a probe.
