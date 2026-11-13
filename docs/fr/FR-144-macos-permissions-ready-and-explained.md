# FR-144: macOS permissions — asked while the person is there, shown at a glance, repaired in one action, and explained to the controller

**Issue:** [#3703](https://github.com/gjovanov/roomler-ai/issues/3703) · **Status:** claimed 2026-11-11. Nothing built. P0 (the field read that records RED) comes first
**Decided by:** the operator, 2026-11-11, for [#3247](https://github.com/gjovanov/roomler-ai/issues/3247)'s item 9
**Builds on:** #3164 (the companion's permission card, the Quit AppleEvent restart), FR-80 ([#1532](https://github.com/gjovanov/roomler-ai/issues/1532), a session with no pixels says why), FR-43 ([#971](https://github.com/gjovanov/roomler-ai/issues/971), one macOS row)
**Related:** #3240 (the macOS 27 table, rows 2, 3, 7, 8 and 9), #3700, #3701, #2892, FR-97, [FR-141](FR-141-one-run-one-row.md)

## Goal

- **A Mac installed once works for remote desktop from then on**, also when nobody is at it: every macOS prompt
  that needs a person is raised while the person is there, and nothing a month later leaves an unattended Mac
  stuck.
- **The person at the Mac sees at a glance what is granted**: one place, a row per permission, its live state, a
  button to the exact pane, and the restart done by Roomler.
- **A controller is never left with a dead session and no reason**: the viewer says what the Mac lacks or is
  asking, in words that say what to do.

AnyDesk's macOS flow is the reference to match, and to beat where its users got stuck.

## Evidence

Anchors are master `ba6a4b985` (2026-11-11). "Code" means read in the tree; "field" means read on a device (the
field log names which read); "inferred" means neither.

### What the operator decided (2026-11-11)

His words: "1-5 do per best UX - check also https://support.anydesk.com/docs/security-permissions-on-macos, also
check reddit conversation: https://www.reddit.com/r/AnyDesk/comments/1ghxt58/missing_screen_capture_permission_macos/,
https://lucidgen.com/en/how-to-install-give-anydesk-permissions-on-mac/". The five were: raise the prompts at
install; the viewer tells the controller; move local-network traffic out of the person's half; Apple's
persistent-content-capture entitlement; accept and document. He chose the experience, not one option, so this
spec designs the whole of it and returns no option to him.

### What macOS asks of `roomlerd`, and when (field, macOS 27.2 unless marked)

| # | macOS says | Raised by | When |
|---|---|---|---|
| 1 | "Screen Recording" / "“roomlerd” would like to record this computer's screen and audio." [Open System Settings] [Deny] | the agent's start-up preflight (code: `agents/roomlerd/src/app.rs:3751-3783`, run at `:3943`; `CGRequestScreenCaptureAccess`, `crates/agent-core/src/tcc.rs:64-66`), which also opens the pane (`app.rs:3772`) | 51 s after the install's password (10 Nov); on 0.4.211 the alert and System Settings cover the wizard's Done page completely (11 Nov) |
| 2 | the switch, then the password, then "“roomlerd” may not be able to record the contents of your screen until it is quit." [Quit & Reopen] [Later]; after Quit & Reopen, Finder: "The application “roomlerd” is not open anymore." [OK] | macOS; the restart is ours (`agents/roomlerd/src/macos_quit.rs:3-74`), Finder's error comes from LaunchServices' reopen, whose process leaves at once (`app.rs:3794`) | every first grant (2 Nov, 10 Nov, 11 Nov) |
| 3 | "Device Control and Data Access" / "“roomlerd” would like to control this Mac and access your data." [Open System Settings] [Deny] | the relaunched agent's preflight (`app.rs:3753`); macOS 27's pane for Accessibility | 1 s after the relaunch, under Finder's false error (10 Nov); it stays on screen after the grant until answered (11 Nov) |
| 4 | "“roomlerd” is requesting to bypass the system private window picker and directly access your screen and audio." [Allow] [Open System Settings] | the first capture of a session; nothing in our code handles it (#3240 row 3) | at the first remote session (2 Nov, 11 Nov); macOS's own record sets the next one 30 days later (11 Nov, read from replayd's file, not seen) |
| 5 | "Allow “roomlerd” to find devices on local networks?" [Don't Allow] [Allow] | the session's network paths (below) | at the first remote session (2 Nov, 11 Nov) |

On 11 Nov, with nobody at the Mac, prompts 4 and 5 came up together the second the session started. The picture
was live throughout and showed them exactly as the Mac's screen did; **one click on Allow through the stream
answered each**. The first click missed: display match handed the screen back on the controller's own pointer
moves and the prompt moved under the pointer (#3701). On 2 Nov the first typed line went nowhere until someone at
the Mac answered them; on 15.7.7 a typed space pressed "Open System Settings" (#3240 row 7).

### What a person and a controller see today

- **The wizard** (code): one paragraph on Done, "One manual step remains: macOS grants Screen Recording and
  Accessibility by hand…", which also says to choose Quit & Reopen and to allow the local-network prompt
  (`agents/roomler-setup/src/front/app.js:1930-1934`). No live state, no button, nothing about prompt 4. Field
  (11 Nov, 0.4.211): it names "Accessibility", which macOS 27 calls "Device Control and Data Access", and it says
  the same after a re-install with both grants on.
- **The companion** (code): a banner shown only while something is missing, with two rows ("Screen Recording —
  the remote screen will be blank", "Accessibility — remote keyboard and mouse will do nothing"), an Open
  Settings button each, and Restart roomlerd (`agents/roomler-desktop/src/front/index.html:102-130`), re-read on
  window focus (`overview.js:656-678`). The grants are the agent's own (`agents/roomler-desktop/src/commands.rs:2402-2409`,
  `agents/roomlerd/src/localapi_state.rs:460-473`). Its Screen Recording read is not live: it stays false after
  the grant until a restart (`crates/localapi/src/lib.rs:537-549`). No row for prompts 4 or 5, no stale state, no
  "Allowed" state. For its first 30 s after the install it says "Not enrolled yet" (#3700).
- **The Devices page** (code): "No screen access" and "No input access" chips (`ui/src/components/admin/agentCodecChips.ts:92-115`)
  from the permission words the agent computes once per process (`agents/roomlerd/src/encode/caps.rs:4450-4479`).
  Field (11 Nov): "No input access" 4 minutes after the Accessibility grant, gone after a restart; and in the
  item 9 session, typed input landed while the row said it had no input permission (#3240 row 8).
- **The viewer** (code): on `rc:media-unavailable` with code `permission`, "This device cannot capture its
  screen." and the agent's sentence (`ui/src/views/remote/RemoteControl.vue:1394-1409`,
  `agents/roomlerd/src/capture/mod.rs:786-790`, corrected for old agents by `ui/src/composables/macPermissionHint.ts:33-39`).
  No signal reaches the viewer for a missing input grant (code: no such message on the wire). Nothing about a
  prompt on the Mac's screen (field, 11 Nov).

### The stale grant (code and field)

- TCC binds a grant to the app's code requirement. When the signing identity changed on 2026-09-08, `tccd` logged
  `Failed to match existing code requirement for subject com.roomler.agent and service kTCCServiceScreenCapture`
  while the entry stayed listed and on (`docs/fr/FR-80-a-session-with-no-pixels-says-why.md:27-33`). A release's
  Developer ID signature keeps the grants across our updates (`docs/installation.md:2245-2247`; field: no prompt
  across the 0.4.200 update, #971's AC4 read).
- The agent cannot tell "never granted" from "stale": both read `false` (`crates/agent-core/src/tcc.rs:56-58`),
  and the viewer's sentence only warns that "A macOS update or a change to the app's code signature invalidates an
  existing grant" (`capture/mod.rs:789-790`).
- Nothing of ours reads the stored grants. The one reset is the uninstall's, as root, while the bundle is there
  (`scripts/install.sh:1943-1951`).

### Bundle, signing and the network paths (code)

- The grants hang on the bundle id `com.roomler.agent` (`agents/roomlerd/src/macos_quit.rs:117-120`,
  `.github/workflows/release-agent.yml:3215`), signed Developer ID with the hardened runtime and
  `agents/roomlerd/packaging/macos/entitlements.plist`, which turns on audio input only
  (`release-agent.yml:3383-3397`). The Info.plist carries usage descriptions for screen capture, Apple Events and
  the microphone, and none for the local network, so prompt 5 has Apple's generic words
  (`release-agent.yml:3209-3230`; `docs/fr/FR-97-os-feature-parity.md:831-836`).
- The person's half runs the session's WebRTC peer. Its ICE resolves a browser's `.local` host candidate with the
  OS resolver, then hands an unanswered one to webrtc-ice's own mDNS resolver, which asks once a second until
  answered (`agents/roomlerd/src/mdns_resolve.rs:7-14`, `:40-44`). A browser off the Mac's network never answers.
  Inferred: those sends are the `Failed to send mDNS packet No route to host (os error 65)` lines that ran twice a
  second while prompt 5 stood and stopped within 50 ms of Allow (field, 11 Nov).
- A root LaunchDaemon is outside Local Network privacy (FR-97, from Apple's TN3179; to verify in the field on 27).

### What AnyDesk does, and where its users got stuck (the operator's references, read 2026-11-11)

- **Its surface:** a status icon beside the AnyDesk ID opens "System Permission Status": Screen Recording,
  Accessibility, Full Disk Access, a button each to its pane, a note that the change "will require a restart of the
  application", the row turning "Granted" while the window is open, then "quit AnyDesk and reopen it". Help has
  "Reset all permissions". Unattended access is a separate service install plus "Allow in the Background".
- **The controller's side:** "Could not start session. Missing screen capture permission on the remote side."
- **Where people got stuck** (the Reddit thread, AnyDesk 8.1.4 on macOS 12.7.6, still collecting "solved" replies
  two years on): after an update the entries were listed and switched on and did not count. What fixed it: delete
  all three entries with the minus sign, restart the app, grant again. Reinstalling did not help. People lost days
  before a forum post told them, and the vendor's page described an older macOS.
- **Not covered by any of the three:** macOS versions, the monthly screen-capture prompt of macOS 15 and later,
  what an update does, a Mac with nobody at it, management profiles.

So the bar: one surface, the restart done for the person, and the stale grant found and repaired by the product
rather than by a forum. To beat it: the controller is told what the Mac is asking and can answer it from the
picture, and the prompts that need a person are raised while the person is there.

## Design

The names a person reads (the pane, the entry) come from one table by macOS major version, the class #3240
proposes for its row 2. Every surface below reads it; if that PR has not landed, P1 lands it.

### (a) The Mac's own permissions surface: the companion, and the wizard's step (P1)

One component, shown by the companion's Overview as a "Permissions" section and by the wizard as a step. A row
per permission `roomlerd` needs:

| Row, as the person reads it | Without it | Live state from | Button |
|---|---|---|---|
| **See the screen** — Screen & System Audio Recording | "The remote screen is blank." | a fresh `roomlerd` child probe, because the running process's read stays false until a restart (`crates/localapi/src/lib.rs:537-549`); that macOS attributes the child to `roomlerd.app` is to verify in the field | "Open Screen & System Audio Recording" |
| **Control the mouse and keyboard** — Device Control and Data Access (macOS 27), Accessibility (earlier) | "Clicks and keys from the controller do nothing." | `AXIsProcessTrusted`, live (`tcc.rs:69-71`) | "Open Device Control and Data Access" |
| **Reach this network** — Local Network | "A controller on the same network connects through the internet or a relay instead of directly." | the agent's own sends: the last result of a multicast or local-address send | "Ask now", then "Open Local Network" once answered |
| **Show the screen without a monthly question** — the screen-capture reminder | "macOS asks again about every 30 days, and the question covers the screen until someone answers it." | the agent's own record of when it last saw the reminder answered (replayd's own record, if a process of ours may read it: to verify in the field) | "Ask now" (one capture) |

Each row's state is one of: **Allowed**; **Off** ("Turn on roomlerd"); **macOS is asking now** ("Answer the
question on screen: Allow"); **On, applies after a restart** (Roomler restarts roomlerd, below); **On, but macOS
refuses it** (design b); **Not asked yet**. Allowed rows stay visible: the person sees what is granted, not only
what is missing.

- **The standing warning.** While any row is not Allowed, the menu-bar icon carries a warning mark and the
  section heads the Overview: "Remote control of this Mac is not ready: <the row's name>." It is computed from the
  rows, so it goes away the moment the last one is Allowed.
- **The restart is ours.** When See the screen turns on, Roomler restarts `roomlerd` itself through the LocalAPI
  `RestartDaemon` verb the card already uses (`overview.js:689-708`), at once when no session is live, and
  otherwise after asking: "Restart roomlerd now? It ends the current remote session." What restarts is the
  process that holds the grant, the person's half: on a Mac with a root half, that half and its private network
  stay up (which process the verb restarts on a one-row Mac is to verify in P1). "Live" counts everything the
  restarted process serves: a remote-control session, an SSH session, a tunnel forward. The step's text before the
  switch: "macOS will offer Quit & Reopen: choose Later. Roomler restarts roomlerd for you." For a person who chooses
  Quit & Reopen anyway, the process LaunchServices starts (`app.rs:3824`) takes LaunchServices' open event before it
  leaves, so Finder has nothing to report (#3240 row 9 was triaged "Discard" as owned by the OS; the operator's
  decision takes it back; that the open event is what Finder waits for is to verify in the field).
- **Where it lives.** The companion's banner (`index.html:102-130`) becomes this section; its "or choose Quit &
  Reopen" line goes. A link under the rows, "How macOS permissions work", opens the page of design (g).

### (b) The stale grant (P3)

- **Recognised two ways.** (1) `tccd`'s own words: the root half, where there is one (a two-row or one-row Mac),
  reads the unified log for `tccd` lines that name `com.roomler.agent` with "Failed to match existing code
  requirement" (a root daemon can read the log: to verify in the field on 27). (2) Our own record: at each start
  the person's half writes which grants it held, with its version, its signing leaf and the macOS build; a start
  that finds a grant it held before now refused, with the version or the macOS build changed, reads as stale.
- **What the row says:** "On in System Settings, but macOS no longer accepts it. This can happen after an update."
  **One action, Repair:** `tccutil reset <service> com.roomler.agent` (through the root half as the uninstall does,
  `install.sh:1948`, or as the person where there is no root half: to verify in the field), then `roomlerd` asks
  again, which lists it in the pane, the person turns it on, and Roomler restarts it. Repair is never offered to a
  controller: a reset only removes, and the grant needs a person. The root half's reset takes one thing from its
  caller, which row: the bundle id and the service names are fixed in the root half, so the verb reaches no other
  app's grants.
- **When it is checked:** at every start of the person's half (where the preflight runs, `app.rs:3973`), so after
  our own update and after a macOS update without a separate trigger; and when a session's capture fails with
  `permission`.

### (c) At install: every prompt raised on purpose while the person is there (P2)

| Path | What the person meets |
|---|---|
| **The wizard** | A "Permissions" step between Install and Done, in front of System Settings' alert instead of under it. The rows of (a), worked top to bottom: See the screen (prompt 1, the switch, the password, "choose Later", our restart), Control (prompt 3), then **"Test the screen"**: `roomlerd` makes one capture, which raises prompt 4 now, with the line "macOS asks whether roomlerd may skip the window picker: choose Allow", then **"Test the network"**: one mDNS query, which raises prompt 5 ("choose Allow"). Done says "This Mac is ready for remote control, also when nobody is at it." only when every row is Allowed, and otherwise names what is left; it never says "One manual step remains" when nothing remains. |
| **The script** (`install.sh`, the served one-liner) | It ends by opening the companion on its Permissions section, and prints the same four rows. `roomler permissions` lists them in Terminal and `roomler permissions --ask` raises the prompts, through the LocalAPI, since the prompts must come from `roomlerd` itself. |
| **A managed fleet** | A profile recipe in the docs (design g). What we expect, all to verify in the field on 15, 26 and 27: a privacy-preferences profile can allow Accessibility ahead of time for `com.roomler.agent` with its code requirement (`codesign -d -r-`); it cannot grant Screen Recording, only let a standard user approve it; the restrictions key `forceBypassScreenCaptureAlert` suppresses the monthly reminder on supervised Macs (macOS 15.1 and later); no profile key answers Local Network. The surface shows what the profile did not grant. |

The new LocalAPI verbs (ask for a grant, make one capture, send one local-network probe, the child probe) are
additive: an older companion or wizard never calls them, an older daemon answers them as unknown, and the surface
then shows today's two rows.

### (d) The controller's side (P4)

| The Mac's state | The viewer says | Signal |
|---|---|---|
| Screen Recording off | "This Mac has not allowed roomlerd to record its screen, so there is no picture. Someone at the Mac must turn it on: Roomler menu-bar app → Permissions." | `rc:media-unavailable` `permission`, which exists (`capture/mod.rs:700-704`) |
| Screen Recording stale | "macOS no longer accepts this Mac's screen permission for roomlerd. This can happen after an update. Someone at the Mac must repair it: Roomler menu-bar app → Permissions → Repair." | the same code with a `stale` detail, additive |
| Control off | "Your clicks and keys do not reach this Mac: roomlerd is not allowed to control it (Device Control and Data Access). Someone at the Mac must turn it on." | a new `rc:` notice from the input arbiter when it drops events for want of the grant (it already re-reads the grant once a second, `docs/remote-control.md:1646-1655`) |
| macOS is asking now, the picture is live | "The Mac is showing a macOS question for roomlerd. Click Allow in the picture." | the agent sees it: Local Network by its own sends failing with `EHOSTUNREACH`; the picker by a window above the captured display at the alert layer, owned by a system process from a per-version table, appearing as capture starts (the owners are to verify in the field per version) |
| A question nobody can reach from here (no picture, a locked screen, the login window) | "macOS is waiting for an answer at the Mac, and it cannot be given from here. Someone at the Mac must answer it." | the same detection with no live picture |

- **#3701 is a dependency**: until display match stops taking the controller's own pointer moves for a person at
  the Mac, the first click on Allow can land beside the button.
- The Devices page's chips follow the OS live (#3240 row 8's PR) and name the pane the Mac's version shows.

### (e) The monthly reminder: Apple's persistent-content-capture entitlement (P5)

- **Applying:** a request to Apple from the publisher's Developer ID team for `com.apple.developer.persistent-content-capture`
  on an explicit App ID `com.roomler.agent`. Apple's answer and its timing are not ours.
- **If granted:** a Developer ID provisioning profile for that App ID carrying the entitlement; the profile
  embedded as `roomlerd.app/Contents/embedded.provisionprofile` before the bundle is sealed; the key added to
  `entitlements.plist`; the release's assertion (`release-agent.yml:3383-3397`) extended to the key and the
  profile. ⚠️ A binary that claims a restricted entitlement without a valid profile does not launch, which would
  take every Mac's daemon down at its next update. Hence the kill switch: the profile step behind a repository
  variable, a lab-only build first, and a release that can be withdrawn. Whether the entitlement covers the capture
  API the agent uses today is to verify in the field.
- **Until then:** the reminder is raised at install (c); the agent records when it was answered; the companion's
  row says "macOS will ask again around <date>"; on that day the viewer says "Click Allow in the picture" (d),
  which the 11 Nov read showed works.

### (f) The local-network prompt: no change to the session's network paths

Decided from the code. The prompt's trigger is the session's own reach into the Mac's network: webrtc-ice's
mDNS queries and, on a shared network, the checks to the browser's local address, which are the direct LAN path
itself. Turning the mDNS resolver off would move the prompt to the first local check and lose the `.local`
candidates the OS resolver did not answer. Moving the peer into the root half is FR-43-sized ICE work on a
field-sensitive path that removes one prompt of four. So the session's paths stay as they are; the prompt is
answered at install (c) or by one click through the picture (d). One change: the bundle gains an
`NSLocalNetworkUsageDescription` (`release-agent.yml:3209-3230`), so the prompt says why: "roomlerd connects
directly to a controller on this network, so remote control from the same network does not go through the
internet."

### (g) Docs (P6)

A page, `docs/macos-permissions.md`: each prompt's words and what to answer, each pane's name by macOS version,
the restart, the stale grant and its repair, the controller's notices, the managed-fleet recipe; it names the
macOS versions it was read on (15.7.x, 26.x, 27.2) and the date. Linked from the companion's "How macOS
permissions work", from the viewer's notices, from `docs/installation.md`'s macOS grants section and from the
public user docs; a row in `docs/README.md`.

## Phases

| phase | what | kill switch | status |
|---|---|---|---|
| P0 | Field read on released builds, recorded RED: a fresh Mac through the wizard (the covered Done page, Finder's false error, the stale Done words); a first session with nobody there (prompts 4 and 5, the viewer's silence); a stale grant (a grant made for a differently signed `roomlerd`); the script install | read only | not started |
| P1 | The names table (#3240 row 2) and the companion's Permissions section (a), with the child probe and the restart; #3240 row 8 if not landed | companion key `macos_permissions_v2`, default on: off = today's banner | not started |
| P2 | The wizard's Permissions step and the LocalAPI ask verbs (c); `roomler permissions`; the usage description (f) | server key `wizard_macos_permissions` in `/api/setup/features`, default off until AC1 holds | not started |
| P3 | The stale grant: both signals, Repair (b) | agent key `macos_stale_grant_check`, default on: off = today | not started |
| P4 | The viewer's notices (d) | agent key `rc_macos_prompt_notice`, default on: off sends none; a viewer shows only what arrives | not started |
| P5 | The entitlement: the application; the signing lane once granted (e) | the profile step behind a repository variable, unset = today's bundle | not started |
| P6 | Docs (g) | none | not started |
| P7 | Field verification on released builds (the criteria below) | none | not started |

## Acceptance criteria

All are field reads on released builds, on throwaway macOS lab guests (27.2, and 15.x where a criterion says so),
unless they say otherwise.

- [ ] **AC1** (a fresh Mac through the wizard, 27.2 and 15.x): every prompt, 1 to 5, comes up while the
  Permissions step is in front; every row reads Allowed before Done; nothing covers the Done page; no Finder
  "is not open anymore"; Done says the Mac is ready and no "manual step".
- [ ] **AC2** (a Mac with nobody there, its first session, after AC1): no macOS question in the picture; the first
  click and the first typed line land.
- [ ] **AC3** (a Mac with nobody there, its first session, prompts 4 and 5 never answered): while each stands the
  viewer says "Click Allow in the picture"; one click through the stream answers it, with #3701 fixed; the notice
  goes.
- [ ] **AC4** (nothing reachable from here): Screen Recording off, the viewer names it and says a person at the
  Mac must turn it on; a question on a locked screen gives the "cannot be given from here" words.
- [ ] **AC5** (control off): the viewer says clicks and keys do not reach the Mac; after the grant the Devices
  page's chip clears within one heartbeat, with no restart.
- [ ] **AC6** (a stale grant after an update): a guest whose grant was made for a differently signed `roomlerd`:
  the row reads "On in System Settings, but macOS no longer accepts it"; Repair, the person's switch and our
  restart bring the picture back; the viewer gave the stale words meanwhile.
- [ ] **AC7** (after a macOS update, if a lab guest can take one): the first start after it runs the check, and
  the rows stay true.
- [ ] **AC8** (day 31, if a guest's clock can show it): the companion gave the date; at the next capture the
  reminder comes up, the viewer says "Click Allow in the picture", one click answers it.
- [ ] **AC9** (the script): the served one-liner ends with the companion's Permissions section in front;
  `roomler permissions` lists the rows and `--ask` raises the prompts.
- [ ] **AC10** (a managed guest, if the lab can enrol one): the docs' profile does what the page says, and the
  surface shows what it did not grant.
- [ ] **AC11**: prompt 5 shows our usage description.
- [ ] **AC12**: on 15, 26 and 27, every surface names the pane that guest shows.
- [ ] **AC13**: docs updated in house style (tables, mermaid where a flow is drawn, `file:line` anchors), the page
  linked from the companion and the viewer, and a row in `docs/README.md`.

## Open decisions

None is open. The operator decided all three on 2026-11-11, each as recommended.

- **D1. Apply for the persistent-content-capture entitlement now? Decided: yes, at once.** Apple's answer takes
  its own time and everything else works without it. Cost: a request under the publisher's Developer ID account,
  a provisioning profile to keep valid in the signing lane, and the launch-failure risk of (e) behind its kill
  switch. The request goes out from the publisher's own Apple Developer account; the field log records it once
  it is in.
- **D2. The restart after Screen Recording: automatic when no session is live? Decided: yes**, and Roomler asks
  only while a session is live. Cost: a restart the person did not press, which ends nothing but the daemon's
  own uptime. The other way, always asking, adds one click to every first install.
- **D3. May the wizard finish before every row is Allowed? Decided: yes**, with "Do this later", Done naming
  what is left, and the standing warning in the companion. Cost: a Mac can leave the wizard not ready for an
  unattended session. Making the step required blocks an install on a person who cannot give a grant (a managed
  Mac, a standard user).

## Out of scope

- How the agent captures and injects; Windows and Linux, which ask nothing of this kind.
- One macOS row (FR-43, [FR-141](FR-141-one-run-one-row.md)); the companion's first-30-seconds words (#3700),
  which P1 does not change; `roomler status` on a two-half Mac (#2892).
- #3240's other rows: the administrator prompt's malware line (1), "App Background Activity" (4), Safari's `.tar`
  (5), the uninstall's leftovers (6).
- Full Disk Access, which AnyDesk asks for file transfer: nothing in a session needs it today (inferred; P0 reads
  whether a file transfer raises any prompt).

## Field log

| date | what | result |
|---|---|---|
| 2026-11-02 | macOS 27.2 lab guest, released `setup-v0.4.197`, the wizard by pairing; the first remote session ([#3022's read](https://github.com/gjovanov/roomler-ai/issues/3022#issuecomment-6578288841), #3247) | RED: prompts 4 and 5 took the focus and the first typed line went nowhere until someone at the Mac answered them; Finder's false error after Quit & Reopen |
| 2026-11-10 | macOS 27.2 lab guest, released `setup-v0.4.209`: the install (#3247 item 4, now #3700) and the first grants (item 10) ([the read](https://github.com/gjovanov/roomler-ai/issues/3247#issuecomment-6799264273)) | RED: System Settings opens over the companion on an empty pane, then prompt 1; after Quit & Reopen Finder's false error lands on top of prompt 3 |
| 2026-11-11 | the same guest, both grants given by hand, one session from the production viewer with nobody at the Mac (#3247 item 9, the same read) | prompts 4 and 5 at once; the picture live; one click on Allow through the stream answered each (the first missed, #3701); the viewer said nothing; input landed while the row said none |
| 2026-11-11 | a fresh macOS 27.2 lab guest, released `setup-v0.4.211`, the wizard by pairing ([the read](https://github.com/gjovanov/roomler-ai/issues/3679#issuecomment-6800327603)) | RED: prompt 1 and System Settings cover the Done page; password, Quit & Reopen, Finder's false error, prompt 3; Done names "Accessibility" and says "One manual step remains", also after a re-install with both grants on; "No input access" until a restart |
