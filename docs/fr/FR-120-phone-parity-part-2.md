# FR-120: Phone parity, part 2 — nothing a phone user needs is hover-only, clipped, or a page away

**Issue:** [#2839](https://github.com/gjovanov/roomler-ai/issues/2839) · **Status:** done, closed 2026-10-25. Every phase is on production:
P5 (#2843) and P2 (#2845) since 2026-10-24 20:34Z (`hosted-20261024-fa90220`), P3 (#2848) since 20:47Z
(`hosted-20261024-9ea48a1`), P1a (#2850) since 22:05Z (with FR-27's `hosted-20261024-46f3f84`), P1b (#2856) since
22:30Z (`hosted-20261024-d96b623`), P6 (#2883) since 2026-10-25 13:59Z (`hosted-20261025-eeb8acd`, the 0.4.170 bump's
image), and P4a (#2887, with P7's docs) and P4b (#2895) since 14:42Z (`hosted-20261025-8a58c53`) · **Owner:** the web app (UI) · **Anchors:** master `30405d173` · **Builds on:**
[FR-116](FR-116-phone-and-tablet-parity.md) (#2561, closed 2026-10-23: the same facts and actions at every width, and
the rules in `docs/ui.md` § Phones and tablets)

## 1. Goal

FR-116 made a phone show the same facts and actions as a desktop. FR-120 makes them **reachable and readable by
touch** on a 360–430 px phone and on a touch tablet:

- **No fact is hover-only.** What a `title=`, a tooltip or a `mouseenter` says on desktop is reachable by a tap. A
  disabled control shows *why* as text.
- **Nothing is clipped by a row that doesn't wrap**, and no dialog with real content is a centred box on a phone.
- **No page is three or four times its desktop length** when a summary with detail on demand would do.
- **A finger can hit what it needs to.** Touch targets of at least 36 px on coarse pointers.

## 2. Evidence

### 2.1 A measured sweep of production (2026-10-24)

**Setup:**
- Playwright 1.62 from mars, with Chromium's mobile emulation (the engine behind DevTools' device mode).
- In the vmtest org as its admin; a throwaway chat room created for the run and deleted after.
- 35 routes (every signed-in route and the public pages) at 360 × 780, 390 × 844 and 768 × 1024 with touch, and at
  1300 × 900 as the desktop reference.
- Production was `hosted-20261024-b78373e` (entry script `index-4xvzWYX1.js`).

| page | 360 px | 1300 px | | what else |
|---|---|---|---|---|
| Devices | 5,377 px | 1,468 px | **3.7×** | 21 targets under 32 px (the cards' Connect buttons are 28 px tall); the status dot's meaning is title-only |
| Exec audit | 5,664 px | 1,698 px | **3.3×** | 18 "Show all" buttons of 49 × 28 px |
| SSH audit | 4,480 px | 1,230 px | **3.6×** | |
| Billing | 2,445 px | 1,093 px | **2.2×** | three plans × nine limit rows, all open |
| Analytics | 3,846 px | 2,881 px | 1.3× | **136 hover-only facts** (the uptime strip's "online · Oct 18 00:16 → 09:53" per segment), **41 truncated** device names |
| Tutorial | 3,789 px | 2,965 px | 1.3× | the chapter rail sits above the content; see §2.3 |
| Sign-in, sign-up | — | — | — | **the document scrolls 4 px sideways** at 360 and 390 px (FR-116's AC2 covered signed-in routes only) |
| every page | | | | primary buttons `size="small"` are **28 px** tall, the dashboard's legend chips **20 px** |

The viewer's files drawer has five icon buttons (Refresh, Close, Parent directory, Upload staging, Drives) with a
`title` and no label.

### 2.2 A code audit of `ui/src` (2026-10-24, master `30405d173`)

About 90 findings, every one against the current code; what FR-116 fixed was checked and left out. The patterns:

| pattern | effect on a phone | examples |
|---|---|---|
| **a disabled control whose reason is its `title`** | a disabled `v-btn` takes no pointer events, so the reason **never shows, on any device** | the viewer's "1:1 Match host display" (`RemoteControl.vue:1674-1679`), its Settings buttons (`:1791`, `:1808`, `:1829`, `:1900`, `:1912`) and clipboard buttons (`:462-491`); Vault's Approve (`VaultApprovalCard.vue:44-50`) |
| **a fact only in `title` or on `mouseenter`** | unreachable by touch | the Devices cards' "needs a person" chips (`AgentsSection.vue:570`, `:594`, `:606`, `:617`, `:628`, `:639`, `:651`, `:663`, `:676`); uptime segments (`UptimeStrip.vue:19`); viewing windows (`UsageTimeline.vue:26`); the mesh's carrier and RTT (`MeshGraph.vue:94-95`, `:112-113`); the viewer's touch-mode help (`RemoteControl.vue:371-373`), refused Record/Privacy (`:693-731`) and a failed transfer (`:818`) |
| **hive has no phone layout at all** | rows squeezed to nothing, headers clipped, centred dialogs | session rows (`HiveSessionsView.vue:125-162`: a chip, a chip and two buttons beside a 0–40 px title), the transcript header (`HiveTranscript.vue:16-37`), the dialogs (`HiveParticipants.vue:13`, `HiveCopies.vue:14`, `HiveCopiesView.vue:269`, `HivePurgeDialog.vue:14`, `HiveManage.vue:16`), memory's three open scopes (`HiveMemoryView.vue:27-139`) |
| **a centred dialog with real content** | ~312 px of form, list or table | five on the Devices page (`AgentsSection.vue:922`, `:1079`, `:1109`, `:1150`, `:1277`), a member's roles (`MembersSection.vue:200-204`), the viewer's agent log (`RemoteControl.vue:2090`, its toolbar leaving the title 0 px), Giphy (`GiphyPicker.vue:4`) |
| **a title or a row that never wraps** | its own button pushed off the card | `v-card-title` questions that name a device (`AgentsSection.vue:924-927`, `:1050`); Overlay routes' title row (`OverlaySubnetRoutesSection.vue:5-27`); Roles' (`RolesSection.vue:5-17`); a joined call's header (`ConferenceView.vue:8-45`) |
| **a list shown whole, or every column stacked** | pages 3–6× desktop | Files, Members, Invites (`FilesBrowser.vue:53-113`: every column of every row stacked); usage's Calls and Tunnel sessions tables (`UsagePanel.vue:90-109`, `:115-138`); Keep busy history (`KeepBusyHistorySection.vue:50-79`); the agent log (`AgentLogsDialog.vue:107-118`, no paging); the room tree (`RoomList.vue:38-42`) |

### 2.3 Bugs found on the way

- **The tutorial's navigation scrolls the wrong thing** (`TutorialView.vue:301-305`). `go()` calls
  `globalThis.scrollTo`, but `v-main` is the scroller (`docs/ui.md` § Responsive page padding). `AppLayout` resets the
  scroll only when the path changes, and `go()` changes the hash. So a tap on a chapter or Next changes the content
  off-screen, and on a phone it looks like nothing happened. Desktop has the same bug once the reader has scrolled.
- **Sign-in and sign-up scroll sideways** by 4 px at 360 and 390 px (§2.1).
- **Three fixed widths don't fit a 360 px screen:**
  - `KeepBusyMenu.vue:42`: `min-width="340"`, inside a menu on a 360 px screen;
  - the uptime strip's 140 px label column (`UptimeStrip.vue:166`);
  - MeshGraph's 320 px minimum (`MeshGraph.vue:238`).
- **Operator tables keep their action in the rightmost column, off-screen at 360 px:**
  - Observability's org table (`ObservabilityView.vue:241-280`, *inspect*);
  - Growth's (`GrowthView.vue:227-274`, *timeline*);
  - the machine keys (`GrowthChannels.vue:227-256`, *revoke*).

## 3. Design

### 3.1 The rules (they go into `docs/ui.md` § Phones and tablets)

1. **A disabled control says why, as text.** A caption under it, or a `v-list-item` subtitle, never its `title`: a
   disabled `v-btn` takes no pointer events, so the `title` reaches nobody.
2. **A fact behind a hover has a tap path on a touch screen** (`(hover: none)`). Options: a chip that opens a menu
   with the reason and the fix; a strip, timeline or graph segment that shows its detail on tap; an icon button with
   an `aria-label`. This extends FR-116's rule 6 from controls to facts.
3. **Touch targets are at least 36 px on a coarse pointer** (`@media (pointer: coarse)`): a hit area, not a visual
   change, so desktop is pixel-identical.
4. **A page at 360 px is at most twice its desktop height for the same data.** Past that, a row or card shows what
   decides, and the rest opens on a tap (FR-116 rule 2, made measurable).

### 3.2 By phase

| phase | what | where (anchors at master `30405d173`) |
|---|---|---|
| **P1** | the tap paths: disabled reasons as text; chips, strips, timelines and graph details by tap; `aria-label` on icon buttons | `RemoteControl.vue`, `VaultApprovalCard.vue`, `AgentsSection.vue`, `UptimeStrip.vue`, `UsageTimeline.vue`, `MeshGraph.vue`, the audit sections' title-only explanations |
| **P2** | hive on a phone: session rows (⋮ on xs, plus a pager), the transcript header, every hive dialog full screen, memory one panel per scope | `HiveSessionsView.vue`, `HiveTranscript.vue`, `HiveParticipants.vue`, `HiveCopies.vue`, `HiveCopiesView.vue`, `HivePurgeDialog.vue`, `HiveManage.vue`, `HiveMemoryView.vue` |
| **P3** | the dialogs and rows FR-116 didn't reach: `useResponsiveDialog()`, titles out of `v-card-title`, title rows that wrap; a joined call's header as the room header | §2.2's rows 4 and 5 |
| **P4** | long pages to at most 2× desktop: compact Devices cards, audit rows, Files/Members/Invites rows (name and state, the rest on tap), Billing's plans, usage tables as `table-stack-xs`, agent log paging, room tree search | `AgentsSection.vue`, the audit sections, `FilesBrowser.vue`, `MembersSection.vue`, `InviteManageView.vue`, `BillingView.vue`, `UsagePanel.vue`, `KeepBusyHistorySection.vue`, `AgentLogsDialog.vue`, `RoomList.vue` |
| **P5** | the bugs: the tutorial scrolls `v-main`; sign-in's sideways scroll; the three fixed widths; the operator tables as `table-stack-xs` | §2.3 |
| **P6** | touch targets of at least 36 px on coarse pointers | `ui/src/styles/responsive.css` |
| **P7** | docs: §3.1's rules in `docs/ui.md`, its diagram and the `docs/README.md` index row | `docs/ui.md` |

## 4. Phases

| Phase | What | Kill switch | Proven on | Size |
|---|---|---|---|---|
| **P1** — P1a #2850 (the facts) · P1b #2856 (the viewer, Vault) | tap paths for hover-only facts; disabled reasons as text | revert: UI only | the sweep's hover-only count on the touched routes, red then green on production | M |
| **P2** — #2845 | hive on a phone | revert: UI only | the hive routes at 360 px in the vmtest org (hive is on there) | M |
| **P3** — #2848 | dialogs and rows | revert: UI only | each dialog opened at 360 px | M |
| **P4** — P4a #2887 (Devices, the audits, Billing) · P4b #2895 (Members, Invites, Files, the usage tables, keep-busy, the agent log, the room search) | long pages | revert: UI only | the sweep's heights, red then green | L |
| **P5** — #2843 | the bugs | revert: UI only | the tutorial at 360 and 1300 px; sign-in's `scrollWidth` | S |
| **P6** — #2883 | touch targets | revert: CSS only | the sweep's small-target count on a coarse pointer; desktop pixel-identical | S |
| **P7** — with P4a, #2887 | docs | — | CI | S |

## 5. Acceptance criteria

- [x] **AC1**: on the swept routes at 360 px, no fact is reachable only by hover. Every meaning-bearing `title` has a
  tap path or visible text, and every disabled control states its reason as text. The sweep's hover-only list is
  empty except decorative titles, each named in §8. *(Field-verified on production. The sweep, 2026-10-24 22:55Z: 0 at
  360, 390 and 768 px on every route. A live session, 2026-10-25 13:52–14:00Z: every off Settings control says why as
  text, and the toolbar's disabled controls say why on a tap; §8.)*
- [x] **AC2**: hive at 360 px: a session row shows its title, device and folder on at least half the row, with its
  actions one tap away. The transcript header's controls are all reachable, and every hive dialog is full screen.
  *(Field-verified on production. On the vmtest org's data, 2026-10-24 20:45Z: 74 % of the row is text, Start and
  People are full screen, and the header wraps. With synthetic hive data for what that org cannot reach, 2026-10-25
  13:44–13:49Z: the row menu, the pager, and the Purge, Edit-a-fact, Add-a-copy, Copies and Manage dialogs, each full
  screen with a close; §8.)*
- [x] **AC3**: every dialog §2.2 lists is full screen on `xs` with its actions reachable, and no title or title row
  clips its own text or button at 360 px. *(Field-verified on production: the Devices dialogs, a member's roles and
  GIFs on 2026-10-24 20:59Z; the viewer's agent log in a live session on 2026-10-25 14:00Z, full screen, with its line
  count and path in the toolbar's second row; §8.)*
- [x] **AC4**: Devices, the exec and SSH audits, Billing, Files, Members and Invites are each at most 2× their desktop
  height at 360 px for the same data (§2.1 is the baseline). *(Field-verified on production 2026-10-25 14:57Z,
  `hosted-20261025-8a58c53`: 1.26–1.73× on all seven, with 20 rows on Files, Members and Invites; §8.)*
- [x] **AC5**: the tutorial's chapter navigation brings the chapter's top into view at 360 and 1300 px. *(Field-verified
  on production 2026-10-24 20:45Z, `hosted-20261024-fa90220`: 486 → 76 px at 360, −1,080 → 76 px at 1300; §8.)*
- [x] **AC6**: no page-level sideways scroll at 360 and 390 px on any public page (sign-in, sign-up, landing, legal).
  *(Field-verified on production 2026-10-24 20:49Z: all five public pages 0 px at both widths; §8.)*
- [x] **AC7**: on a coarse pointer, every primary button and chip on the swept routes has a hit area of at least
  36 px, with desktop pixel-identical at 1300 px. *(Field-verified on production 2026-10-25 14:40Z,
  `hosted-20261025-eeb8acd`. Of 303 targets at 360 px and 361 at 768 px, none is under 36 px, crowded, or taking
  another's tap, on any route or in a live session's toolbar. At 1300 px the rule touches none of 804 controls; §8.)*
- [x] **AC8**: the operator tables (Observability, Growth, machine keys) keep their action on screen at 360 px.
  *(Field-verified 2026-10-25 14:11Z on production's bundle with synthetic operator data, since the vmtest admin is no
  operator: every `inspect`, `timeline` and `revoke` button is on screen; §8.)*
- [x] **AC9**: Docs: `docs/ui.md` states §3.1's rules (its diagram updated), and the `docs/README.md` index row covers
  them. *(P1b, P6 and P4a's rows in § Phones and tablets; its breakpoint diagram gains the touch screen and the list
  decision the 2× bound; the index row names all four. #2887)*

## 6. Decisions

All four were decided by the operator on 2026-10-25, each as recommended.

| | decision | resolution |
|---|---|---|
| **D1** | the touch-target minimum: 36, 40 or 44 px | **36 px**: Vuetify's default button height, so a small button's hit area grows to it and nothing else moves (P6) |
| **D2** | compact Devices cards: a one-line summary row that expands, or the FR-116 card with fewer open facts | **a summary row** (name, status, OS, one action), the card on a tap, because the card is what made Devices 3.7× (P4) |
| **D3** | FR-116's D3, carried over: the touch presets above a short landscape stage | **out of FR-120**: still a product call, with no layout picked |
| **D4** | the viewer's Settings is full screen below 1280 px (it was bound to `mobile`), so on a tablet and a laptop window too | **a phone only**: below 600 px wide, or below 600 px tall (a phone held sideways), which is what rc.199 meant by "fullscreen on phones"; above that, the 560 px card leaves the stage in view (P6) |

## 7. Out of scope

- Native apps, offline mode, a PWA install flow.
- Redesigning desktop layouts beyond parity.
- Server behaviour: FR-120 is presentation only.
- The viewer's input, transport, consent and session logic.

## 8. Field-verification log

| date | build | host | result |
|---|---|---|---|
| 2026-10-24 18:17–18:30Z | production, `hosted-20261024-b78373e` (entry `index-4xvzWYX1.js`) | the vmtest org from mars (§2.1's instrument); a first run died at 18:06Z when mars's `roomlerd` updated to 0.4.165 under it, and this one ran in a transient systemd unit | 🔴 **the baseline**: §2.1's numbers. Sign-in scrolls sideways; Devices 3.7× and the audits 3.3–3.6× desktop; analytics 136 hover-only facts; 28 px buttons |
| 2026-10-24 18:47–18:48Z | P5 (#2843) before merge: production (`index-4xvzWYX1.js`) vs the branch's `ui/dist` served at the production origin (`index-Mv5fx5ME.js`) | the vmtest org from mars | 🔴 → 🟢 **AC6**: sign-in and sign-up scroll 4 px sideways at 360 and 390 px on production; 0 on the branch. **AC5**: a chapter tap at 360 px leaves the chapter's top at 486 px on production and brings it to 76 px on the branch. At 1300 px, after scrolling the chapter to −1,080 px, a click leaves it at −992 px on production and brings it to 76 px on the branch. A click while the chapter is in view does not jump on either |
| 2026-10-24 19:18–19:19Z | P2 (#2845) before merge: production (`index-4xvzWYX1.js`) vs the branch (`index-CDr5ZGBf.js`) | the vmtest org from mars: 42 agent sessions (refused or ended, some adopted), an empty memory | 🔴 → 🟢 **AC2**: a row's title and subtitle get 30–56 % of the 344 px row on production, 74 % on every row on the branch. The Start and People dialogs go from 312 px cards to full screen with a close. The transcript header's status chip goes from 46 px and its reason from 30 px to wrapping, both shown in full. Memory goes from three open cards (1,112 px, and v-main overflowing by 4 px) to three closed panels (428 px, no overflow). That overflow was clipped (`overflow-x: hidden` on v-main), so nothing scrolled; the first write-up called it a scroll, corrected on #2839. Not reachable in this org, so covered by unit tests: the row menu (no live session) and the pager (42 sessions, fewer than 50) |
| 2026-10-24 19:41–19:42Z | P3 (#2848) before merge: production (`index-4xvzWYX1.js`) vs the branch (`index-BUiW1ZBa.js`) | the vmtest org from mars; dialogs opened only, each closed with Escape; one throwaway room made and deleted | 🔴 → 🟢 **AC3**: the Devices page's evict, update-all, delete, add-to-an-org and routes dialogs, a member's roles and GIFs go from 312 px cards (only GIFs had a close) to full screen, each with a close. The evict question was clipped before the device's name began, and rotate-key's partway through it; both now wrap. Overlay routes' device chip goes from 58 px to 117 px. Roles' title row clips **New Role** at 320 px on production and wraps on the branch; it is one line at 360 px on both. A call's header was measured and left as is: before joining, the name gets 136 px beside a 136 px spacer, and a joined call's two extra 48 px buttons fit inside that spacer |
| 2026-10-24 20:43–20:44Z | P1a (#2850) before merge: production after tonight's roll (`index-BALEqlEN.js`) vs the branch (`index-D2E_XMBH.js`) | the vmtest org from mars; read-only | 🔴 → 🟢 **AC1, the facts half**: the Devices page goes from 10 title-only status dots and 19 chips whose title neither opened nor acted to 0 and 0, and a tap on a chip opens its explanation. Analytics goes from 498 title-only uptime segments to 0; the bar goes from 118 to 312 px of its row, and a tap reads the period into the line below. The mesh graph goes from 320 px in a 312 px box with no hit line to 312 px with three hit lines, and a tap pins a carrier's line. The dashboard's v-main overflows by 4 px on both builds: its rows' gutters reach past `pa-2`. v-main clips that (`overflow-x: hidden`), so nothing scrolls. The first write-up called it a scroll, corrected on #2839. The viewing windows have no data in this org, so unit tests cover them. A wrong turn, recorded: the first branch run clicked raw coordinates below the fold (`elementFromPoint` was null); a tap scrolled into view reads the period |
| 2026-10-24 20:45–20:49Z | production, `hosted-20261024-fa90220` (entry `index-BALEqlEN.js`): P5 and P2 live (roomler-hive's roll; promote run 40130081897, pods on it since 20:34Z) | the vmtest org from mars; signed out for the public pages | 🟢 **AC5**: a chapter tap at 360 px brings the chapter's top from 486 to 76 px; at 1300 px, after scrolling it to −1,080 px, a click brings it to 76 px; no jump while it is in view. **AC6**: 0 px sideways at 360 and 390 px on the landing page, sign-in, sign-up, terms and privacy. That run was at 20:49Z, by then on 9ea48a1's build, but P3 changed no public page. **AC2**: rows give 74 % of the 344 px row to text; Start and People are full screen with a close; the transcript header wraps with every control on screen; memory shows three closed panels at 428 px; 1300 px is unchanged. Still unproven in the field: the row menu (no live session here), the pager (42 sessions, fewer than 50), and the Copies, Purge, Manage, Add-a-copy and Edit-a-fact dialogs (they need a copied org or a kept fact; their structure is unit-tested) |
| 2026-10-24 20:59Z | production, `hosted-20261024-9ea48a1` (entry `index-I_6uz4_G.js`): P3 live (promote run 40131310876, pods on it since 20:47Z); `/health` ok at 0.4.166; the vmtest org had 4 devices online before and 4 after | the vmtest org from mars; dialogs opened only; one throwaway room made and deleted | 🟢 **AC3**, the same results as the branch before merge: the five Devices dialogs, a member's roles and GIFs are full screen with a close; the evict and rotate-key questions wrap with the device's name visible; the routes chip is shown whole; Roles is one line at 360 px and wraps at 320 px; at 1300 px the dialog is a centred card. Not field-run: the viewer's agent-log dialog, which needs a live session (`fr120P3Dialogs.spec.ts` locks its structure) |
| 2026-10-24 21:16–21:17Z | P1b (#2856) before merge: production (`index-I_6uz4_G.js`) vs the branch (`index-VYJBCEQ1.js`) | the vmtest org from mars; an offline device's viewer, so no session starts | 🔴 → 🟢 **AC1, the viewer half**: title-only elements go from 5 to 0 (Refresh, Close, Parent directory, Upload staging folder, Drives / roots), and the files drawer's unnamed icon buttons from 5 of 6 to 0 of 6. The Settings captions and the toolbar tap paths (clipboard, Record, Privacy) appear only in a live session, so the idle viewer shows none of them; `fr120P1bViewer.spec.ts` locks their structure, and a field check is owed with the next remote-control field run |
| 2026-10-24 22:18Z | production, `hosted-20261024-46f3f84` (entry `index-CdwriITG.js`): P1a live as a passenger of FR-27's roll (its promote finished 22:16Z; pods on it since 22:05Z) | the vmtest org from mars; read-only | 🟢 **AC1, the facts half**, the same results as the branch before merge: Devices has 0 title-only elements and 0 hover-only chips, and a tap on a chip opens its explanation; Analytics has 0 title-only elements, its bar spans 312 of 312 px, and a tap reads the period; the mesh graph is 312 px in its 312 px box with three hit lines, and a tap shows the carrier |
| 2026-10-24 22:42Z | production, `hosted-20261024-d96b623` (entry `index-DeEzDs4P.js`): P1b live (promote run 40138715145, pods on it since 22:30Z); `/health` ok at 0.4.166; the vmtest org had 4 devices online before and 4 after | the vmtest org from mars; an offline device's viewer | 🟢 **AC1, the viewer half**, the same results as the branch before merge: the viewer has 0 title-only elements, and all six of the files drawer's icon buttons are named |
| 2026-10-24 22:42–22:55Z | production, `hosted-20261024-d96b623` (entry `index-DeEzDs4P.js`), with every FR-120 phase so far live: P5, P2, P3, P1a and P1b | §2.1's instrument re-run from mars: 35 routes at 360, 390, 768 and 1300 px (140 records); one throwaway room, deleted (200) | 🟢 **AC1, its measured half**: title-only elements at 360, 390 and 768 px are **0 on every route**. §2.1's baseline had 136 on Analytics, 10 on Devices and 5 on the idle viewer. At 1300 px the only ones left are the Devices grid's 10 presence dots, which are decorative: the status chip in the same row carries the same words and opens them on a tap. Still owed for AC1: a field check, in a live session, of the viewer's Settings captions and toolbar tap paths (locked by `fr120P1bViewer.spec.ts`) |
| 2026-10-25 11:38–11:55Z | P6 before merge: production (`hosted-20261025-033df9d`, entry `index-gJmkfi-I.js`) vs the branch (`index-DnkKDM-_.js`), loaded one after the other for each route | §2.1's 35 routes from mars, at 360 and 768 px (touch) and 1300 px; one throwaway room, deleted (200) | 🔴 → 🟢 **AC7, a first pass**: at 360 px, targets under 36 px go from 43 to 0 on the rooms list, 32 to 9 on Devices, and 5 to 0 on the tutorial and a room. On every signed-in page production also gives a tap inside Search's own box to the unread badge over its corner; the branch gives none. At 1300 px no control is touched (0 isolated on every route). Each difference between the two loads there is live data (an unread count going 6 → 5, the drawer's badges, uptime segments) or the landing and pricing pages, which production serves as static HTML outside the app bundle. **Two wrong turns, recorded:** the run outpaced the API's 60 requests a minute, so some lists loaded blank and a "Too many requests" snackbar sat over the audits' Show more; and its probes, 17.5 px from a centre, sat half a pixel inside hit areas that end exactly where the next box begins (a Devices card's x-small chips above its Access panel, Members' Roles above its divider). A 2 px, then a 1 px probe showed both at exactly 36 px, and the probe now allows that pixel |
| 2026-10-25 12:05–12:24Z | P6 before merge, paced at 13 s between loads (no load throttled): production vs the branch (`index-CvOqTfG5.js`, then `index-eFVM7XK5.js`) | the vmtest org from mars: the ten routes with a small target, at 360 and 768 px | 🔴 → 🟢 **AC7**: the branch has 0 targets under 36 px on all ten. Devices goes 32 → 0 (35 → 0 at 768), the rooms list 43 → 0, a room 5 → 0, Members 2 → 0, each audit 1 → 0, the viewer 1 → 0, sign-in and sign-up 5 → 0 at 768, and the dashboard 5 → 0 (6 → 0). No target takes another's tap. Every page's boxes match production's except the dashboard's, where the carrier filter's chips now have 10 px above and below on a touch screen. Their chip group scrolls sideways, so it clipped their hit areas to 28 px; at 8 px the two rows' hit areas abutted and split their edge a pixel off-centre |
| 2026-10-25 11:51–11:54Z | P4a before merge, a first pass: production (`index-gJmkfi-I.js`) vs the branch (`index-DiyeYaTU.js`) | the vmtest org from mars; read-only | **A wrong turn, recorded:** the run outpaced the API's 60 requests a minute, so lists came back blank. Production's exec audit measured 780 px because its list never loaded, and one build's Devices page lost its enrollment keys. The paced runs below replace it |
| 2026-10-25 12:24–12:58Z | P4a before merge, paced at 13 s between loads (no load throttled): production (`hosted-20261025-1acc44e`, entry `index-gJmkfi-I.js`) vs the branch (`index-DiyeYaTU.js`, then `index-BRWjj9YW.js` with the keys, then `index-DrhylERg.js` with Billing) | the vmtest org from mars; read-only, the taps open and close rows | 🔴 → 🟢 **AC4** at 360 px, against desktop heights that did not change: Devices 5,377 → 1,851 px (3.66× → 1.26×), the exec audit 5,664 → 2,934 px (3.34× → 1.73×), the SSH audit 4,480 → 1,990 px (3.64× → 1.62×) and Billing 2,445 → 1,665 px (2.24× → 1.52×). Files, Members and Invites measure 0.87× on both builds: the vmtest org has almost none of each, so P4b measures them with rows. With only the cards folded, Devices came to 3,669 px (2.5×); the rest was the enrollment keys under them, which then got a line of their own. A phone shows ten 50 px device lines; a tap opens one card, with its panels and actions, and leaves the others closed, and a second tap closes it. An exec card goes from 128 to 276 px when opened (Who appears only then), an SSH card from 68 to 194 px. A tablet's Devices cards (768 px) match production box for box |
| 2026-10-25 13:11–13:25Z | P4b before merge: production (`hosted-20261025-1acc44e`, entry `index-gJmkfi-I.js`) vs the branch (`index-BOFDHIL2.js`, then `index-YZigAQaZ.js`), with the same 20 synthetic members, invites and files served to both builds from the page itself, so nothing reached the server | the vmtest org from mars, paced at 13 s between loads; the vmtest org has almost none of each, so its own pages measured 1× | 🔴 → 🟢 **AC4, with rows**, at 360 px against desktop heights that did not change (1,058, 1,128 and 1,033 px): Files 4,917 → 1,357 px (4.65× → 1.28×), Members 4,296 → 1,520 px (3.81× → 1.35×), Invites 4,876 → 1,401 px (4.72× → 1.36×). A row is 49 px closed; a tap opens it (to 129, 149 or 98 px) and a second tap closes it. The room search finds 18 of the org's 87 rooms for "e", each with its path. **A wrong turn, recorded:** the first branch build kept Vuetify's `v-data-table__tr--mobile` on the phone row, and that class makes each cell a two-column grid. The line was squeezed into half the width ("An/yo/ne") and the opened details sat beside it, so Invites still read 2.26×. The screenshot showed it, and the row no longer carries the class |
| 2026-10-25 13:33–13:35Z | D4 (in P6) before merge: production (`index-gJmkfi-I.js`) vs the P6 branch (`index-eFVM7XK5.js`) | the vmtest org from mars: an offline device's viewer, Settings opened from its toolbar | 🔴 → 🟢 **D4**: a tablet (768 × 1024) went from a full-screen Settings (768 × 1024) to the 560 px card (560 × 405), with the stage in view. A phone held upright (360 × 780) and one held sideways (844 × 390) keep the full screen on both builds; a 1300 px desktop keeps the card on both |
| 2026-10-25 13:44–13:49Z | production (`hosted-20261025-1acc44e`, entry `index-gJmkfi-I.js`) with SYNTHETIC hive data, served from the page itself: 60 of my sessions, three running, in an org whose sessions are copied; two kept facts; a policy; and a real hive room of the vmtest org whose record was served with copies (adopted, ended) | the vmtest org from mars at 360 × 780; nothing reached the server but the reads the pages make anyway, and the one write (minting a token for an archive replica) was answered locally with a placeholder | 🟢 **AC2, what the vmtest org cannot reach**: each phone row's ⋮ menu offers Stop and Delete for good; the pager reaches page 2 (sessions 50 to 59); Delete for good, Edit a fact, Add an archive replica, and the transcript header's Copies and Manage each open full screen (360 × 780) with a close. **A wrong turn, recorded:** the first dialog check looked for `.v-dialog` inside the overlay, but Vuetify puts that class on the overlay itself; the screenshots showed the dialogs open |
| 2026-10-25 13:52–14:00Z | production (`hosted-20261025-1acc44e`, entry `index-gJmkfi-I.js`), live sessions to this run's own kept vmtest cell (an ephemeral Ubuntu guest, `ubuntu/script/system` on zeus, promptless because the vmtest admin enrolled it) | the vmtest org from mars at 360 × 780 with touch; each session ended when its page closed | 🟢 **AC1, the live half**: Settings opens full screen, with four tabs. Every off control says why, as text: Match remote display (it needs an X11 desktop), Host audio and Corp relay assist (both until the next Connect), and clipboard auto-sync (a root daemon has no clipboard). On the toolbar, the disabled clipboard buttons say why on a tap; a refused Record says the owner hasn't allowed remote recording; the touch-mode control says how the new mode works ("Direct touch: the finger is the pointer."). 🟢 **AC3, the agent log**: Settings → Agent logs opens full screen; the title ("Agent log", the device's name after it shrinking with an ellipsis) keeps its refresh, copy and close buttons on screen, and the line count and the log's path ride the second toolbar row. **Two wrong turns, recorded:** the first run decided "connected" from the word Disconnect, which is an icon; and a regex's `\b` arrived on mars as a backspace |
| 2026-10-25 14:09–14:11Z | production (`hosted-20261025-eeb8acd`, entry `index-DgzwBkIm.js`) with SYNTHETIC operator data: in this headless browser only, `/api/auth/me` answered with `is_platform_admin: true` (the client's own gate) and every `/api/admin/` read answered locally, so no operator request reached the server | the vmtest org's admin from mars at 360 × 780 | 🟢 **AC8**: every operator table's action is on screen: Observability's 12 `inspect` buttons end at 87 px of 360, Growth's 12 `timeline` buttons at 107 px, and the machine keys' 3 `revoke` buttons at 101 px. **A wrong turn, recorded:** the first stub answered the channels report with `{enabled: false}`, and the panel, which renders whenever a report exists and reads its `platforms`, went blank |
| 2026-10-25 14:05–14:40Z | production, `hosted-20261025-eeb8acd` (entry `index-DgzwBkIm.js`): P6 live, in the 0.4.170 bump's image, promoted by roomler-ai-4c (pods on it since 13:59Z); `/health` ok at 0.4.170; the vmtest org had 4 devices online before, and 5 after (the 5th my kept cell) | §2.1's 35 routes from mars at 360 and 768 px (touch) and 1300 px, paced at 13 s between loads (none throttled); one throwaway room, deleted (200) | 🟢 **AC7**: every enabled button and tappable chip carries its hit area: 303 targets at 360 px and 361 at 768 px, with **0 under 36 px, 0 crowded and 0 taking another's tap** on every route. At 1300 px the rule touches none of the 804 controls. The landing and pricing pages are static HTML outside the app bundle, with no Vuetify buttons or chips. 🟢 **D4**: Settings is full screen on a phone held upright (360 × 780) or sideways (844 × 390), and the 560 px card on a tablet (768 px) and a desktop |
| 2026-10-25 14:39–14:41Z | production, `hosted-20261025-eeb8acd`, a live session to the kept vmtest cell at 360 × 780 (touch) | the vmtest org from mars | 🟢 **AC7, the session's toolbar**: all 15 of the connected viewer's buttons and chips carry the hit area. 14 were probed (one sat at the screen's edge), with 0 under 36 px, 0 crowded and 0 taking another's tap. The cell was then ended cleanly: its daemon stopped, so the ephemeral device removed itself (its row is gone), and only that VM was destroyed |
| 2026-10-25 14:51–14:57Z | production, `hosted-20261025-8a58c53` (entry `index-D2-0XM-5.js`): P4a (#2887) and P4b (#2895) live (promote run 40203447488, pods on it since 14:42Z); `/health` ok at 0.4.170; the vmtest org had 4 devices online before and 4 after | the vmtest org from mars, paced; Files, Members and Invites with the same 20 synthetic rows as before merge | 🟢 **AC4**, the same numbers as the branch before merge. At 360 / 1300 px: Devices 1,851 / 1,468 px (1.26×), the exec audit 2,934 / 1,698 (1.73×), the SSH audit 1,990 / 1,230 (1.62×), Billing 1,665 / 1,093 (1.52×), Files 1,357 / 1,058 (1.28×), Members 1,520 / 1,128 (1.35×) and Invites 1,401 / 1,033 (1.36×). A phone shows ten device lines, and a tap opens one card's panels; the first exec card opens; a plan goes from 3 to 9 limits on a tap; a member's row opens its details |

## 9. Related

- FR-116 (#2561): part 1. Its rules and harness carry over, and its D3 is carried here as D3.
- FR-90 (#1827): hive, whose views P2 lays out. Presentation only; FR-90's semantics untouched.
- FR-115 (#2557): billing, whose plans P4 folds.
