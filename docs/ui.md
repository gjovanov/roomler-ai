# Frontend

Vue 3 + Vuetify 3 SPA — Vite 7, TypeScript, Pinia (setup-store pattern),
vue-router, vue-i18n (wired, but English-only today). *As of 0.3.0-rc.381.*
Plugin order in `main.ts`: i18n → vuetify → pinia → router.

```mermaid
flowchart LR
    subgraph views["Views (ui/src/views/)"]
        L["LandingView · auth/ · legal/"]
        CORE["dashboard/ · rooms/ · chat/<br/>conference/ · files/ · invite/<br/>profile/ · billing/"]
        FLEET["devices/ · remote/<br/>network/ · observability/<br/>analytics/ · admin/"]
    end

    subgraph state["Pinia stores (24)"]
        S1["auth · tenant · rooms ·<br/>messages · conference · files ·<br/>invite · members · role ·<br/>notification · tasks · user · ws ·<br/>capabilities · setupFeatures · growth"]
        S2["agents · devices · tunnelClients ·<br/>tunnelPolicies · overlayRoutes ·<br/>overlayAcl · orgBadges · stats"]
    end

    subgraph rt["Real-time & media"]
        WS["useWebSocket → /ws"]
        MS["mediasoup-client<br/>(conference)"]
        RC["useRemoteControl<br/>(remote desktop viewer)"]
        WK["decode workers<br/>webcodecs · hevc · vp9-444"]
    end

    views --> state
    CORE --> WS & MS
    FLEET --> RC --> WK
    state --> WS
```

## Views

| Area | Views | Notes |
|---|---|---|
| Marketing / auth | `LandingView`, `auth/{Login,Register,OAuthCallback}View`, `legal/{Terms,PrivacyPolicy}View` | |
| Collaboration | `dashboard/{DashboardView,TenantDashboard}`, `rooms/{RoomList,ExploreView}`, `chat/ChatView`, `conference/ConferenceView`, `files/FilesBrowser`, `invite/{InviteLanding,InviteManage}View`, `profile/{Profile,ProfileEdit}View`, `billing/BillingView` | `ChatView`/`ConferenceView` own their layout (no `v-container`) |
| Fleet | `devices/DevicesView` (enrolled machines), `remote/RemoteControl` (the viewer), `remote/ConsentView`, `network/NetworkPanel` (overlay mesh), `observability/ObservabilityView`, `analytics/AnalyticsView`, `admin/AdminPanel` | |
| Fallback | `NotFoundView` | |

## Stores (24)

`auth` · `tenant` · `user` · `rooms` · `messages` · `conference` · `files` ·
`invite` · `members` · `role` · `notification` · `tasks` · `ws` — the
collaboration core — plus the fleet set: `agents`, `devices`, `tunnelClients`,
`tunnelPolicies`, `overlayRoutes`, `overlayAcl`, `orgBadges` (multi-org
indicators), `stats` (observability series). Three more: `capabilities` (the
module gate, which fails open), `setupFeatures` (the setup page's switches, which
fail closed) and `growth`.

## Composables

| Composable | Purpose |
|---|---|
| `useAuth` | Session + token lifecycle |
| `useWebSocket` | The `/ws` connection + event dispatch into stores |
| `useRemoteControl` | The entire remote-desktop viewer engine (below) |
| `useRemoteRecording` | The viewer's half of the session's `record` channel: start/stop, the device's list, a checked, resumable download ([recording.md](recording.md) §10) |
| `useConferenceLayout` / `useActiveSpeaker` / `useAudioPlayback` / `usePictureInPicture` | Conference UX |
| `useMarkdown` | markdown-it + DOMPurify rendering |
| `usePush` | Web-push subscribe/unsubscribe. #2817 — **an opt-in**: a sign-in or page load only re-subscribes where the browser already granted permission (`shouldSubscribeOnLoad`). The prompt comes only from `enablePushNotifications`, behind the notification panel's *Turn on* click. A browser that blocked it is told where to undo that. #2827 — clicking a push notification (`public/sw.js`) focuses a window **already on** its page, and otherwise opens a new one. It never navigates an open window away: the first roomler window may be a live remote session or a call, and on 2026-10-24 a consent request's own toast hung up the session it was asking about. |
| `usePageViews` | SPA route-change beacon (`/api/stats/pageview`) |
| `usePolling` | Shared polling helper |
| `useSnackbar` / `useValidation` | UX utilities |

## The remote-desktop viewer (`useRemoteControl.ts`)

The largest module in the frontend — the browser side of the pipeline documented in
[encoders.md](encoders.md).

**Render paths** (user-switchable, persisted, feature-probed):

```mermaid
flowchart TB
    IN["incoming session"] --> Q{"transport ×<br/>codec support probes"}
    Q -->|default| V["&lt;video&gt; element<br/>RTP → Chrome jitter buffer (~80 ms floor)"]
    Q -->|"WebCodecs path"| W["RTCRtpScriptTransform →<br/>VideoDecoder → OffscreenCanvas"]
    Q -->|"DC: VP9 · AV1 · H.264 · HEVC"| DC["video-bytes DataChannel →<br/>rc-dc-worker decode → canvas"]
    W & DC --> HUD["per-hop stats: fwd · decode · paint<br/>(rc-hop-stats, opt-in diag HUD)"]
```

- **Decode workers** (`ui/src/workers/`): `rc-webcodecs-worker` (the RTP transform)
  and `rc-dc-worker`, the one worker for every codec over the data channel (two
  until FR-135 R3:
  [remote-control.md §11.14](remote-control.md#1114-one-data-channel-worker-fr-135-r3)),
  with `rc-hop-stats` shared hop-timing instrumentation. Each
  tells its decoder the agent's BT.601 limited colour space through
  `rc-decoder-config` (FR-130 P2; kill switch `roomler-rc-colorspace=off`):
  [remote-control.md §18.6.2](remote-control.md#1862-the-decoder-is-told-the-colour-space-fr-130-p2).
  The data-channel worker also keeps `rc-media-window`: decode, paint, hop,
  arrival and key-frame-gate times in the agent's log-2 buckets, the freeze and
  painted counts, and whether the decoder is hardware, folded into its 1 s `stats`
  message for the agent (FR-135 R0,
  [remote-control.md §11.13.1](remote-control.md#11131-the-viewers-half-r0c)).
  The page relays the `video-bytes` channel to it, one `postMessage` per
  message; under `roomler-rc-dc-worker=1`, where the browser can transfer an
  `RTCDataChannel`, the page moves the channel itself into the worker instead,
  and a page whose move the worker does not confirm relays from then on.
  The default is the relay until FR-135's S11 reads the move
  ([remote-control.md §11.14.1](remote-control.md#11141-the-channel-in-the-worker-roomler-rc-dc-worker)).
- **Support probes** per codec (H.264/HEVC/AV1/VP9): software + hardware decode
  detection; HEVC-over-DC is offered only when *agent hardware encode × viewer
  hardware decode* both hold.
- **Transport control**: auto-pick, priority dial (balanced / sharper / smoother),
  keyframe requests, decoder-stats feedback to the agent (`rc:decodestat` — drives
  its frame-skip rate control, and since FR-135 R0 also carries the worker's media
  window, which only records; since R3 also `queue_arr_ms`, the transit queue read
  at arrival beside `queue_ms`, which no agent reads yet:
  [remote-control.md §11.14.2](remote-control.md#11142-the-arrival-reading-queue_arr_ms)),
  one `rc:decode-error` per decode error, counted into
  the session's stats (FR-132 P1,
  [remote-control.md §11.10](remote-control.md#1110-the-viewers-decode-errors-fr-132-p1)),
  a data-channel decoder that fails after its first frame replaced in place, off with
  `roomler-rc-decode-recover=0` (FR-135 R1,
  [remote-control.md §11.11](remote-control.md#1111-a-decode-error-recovers-in-place-fr-135-r1)),
  agent-local loopback-TURN probe for fast relayed
  paths on the same LAN.
- **Input**: HID-mapped keyboard (keyboard-lock, Ctrl-Alt-Del / SAS chord),
  letterboxed + direct coordinate normalization, multi-monitor layout + display
  match, resolution/scale control. The key listener is on the `window` in the
  capture phase, so a key typed into one of the viewer's **own** inputs — the
  files drawer's path field, a dialog's field — reaches it first and it steps
  aside (`isViewerOwnInput`, #2869); the stage and the soft keyboard's
  textarea (`MobileKeyboard.vue`, `data-rc-capture`) are the host's and keep
  forwarding — and for the textarea every key has ONE owner (#2888): a HID
  frame is the listener's, forwarded with its default prevented and its
  propagation stopped; a printable character is the textarea's, typed through
  its `input` handler, where the toolbar's sticky Ctrl / Alt / Win become a
  chord; what the listener drops (an IME keydown, the OS keyboard's own
  code-less keys) the component's `input` / `compositionend` / `keydown` path
  carries — so a hardware keyboard beside an open soft keyboard types once
  (`routeKeyEvent` / `applyKeyRoute` / `routeDeferredPaste`, pure; proven in a
  real Chromium by `ui/e2e-harness/`, [testing.md](testing.md)).
- **Clipboard bridge**: text / HTML / image / native formats, chunked over a DC,
  hash + echo-gate dedupe, auto-sync on focus; optional local-agent loopback
  bridge for full-fidelity RTF.
- **File transfer**: chunked uploads/downloads, folder download (streamed zip),
  resumable, cancellable; the Remote files drawer browses the host — its roots
  view is the empty path (the drives on Windows, a single `/` on Unix), and `/`
  itself is a directory (`isRootsRequest`, #2870).
- **Recording** (FR-85): Record, the REC chip and the device's recordings,
  over a `record` channel opened only when the grant kept `RECORD`; a stripped
  grant shows a disabled control that says why
  ([recording.md](recording.md) §10).
- **Remote apps**: list / focus / launch on the controlled host — and, on a
  host that cannot, the reason ([`remote-apps.md`](remote-apps.md)).
- **Diagnostics**: stats polling, jank detector, long-task observer, inbound-RTP
  diagnostics, agent-log fetch — surfaced in an opt-in HUD.
- **Resilience**: reconnect with backoff, decode-pressure shedding.

### The frame-rate picker (FR-119)

The Video tab of the viewer's settings offers **Frame rate**, with 60, 120 and
144, only for an agent whose hello carries `fps-high`
(`views/remote/RemoteControl.vue:1770`). The agent's side, the grant and its
step-down, is in [`remote-control.md` §5.4](remote-control.md#54-above-60-fps-the-viewer-asks-the-host-proves-it-fr-119).

- **The options** (`frameRateOptions`, `composables/frameRate.ts:52`): 60 is
  always on. 120 and 144 are disabled, with the reason as text, when the host
  reported a screen slower than the rate ("the host's screen refreshes at
  60 Hz").
- **The pick** is stored per browser (`roomler-rc-fps`,
  `composables/frameRate.ts:30`) and sent again each time the control channel
  opens (`sendFrameRatePreference`, `composables/useRemoteControl.ts:7928`), so
  a reload asks again without a click.
- **The hint under it is the agent's answer**, from `rc:video-info`
  (`frameRateSummary`, `composables/frameRate.ts:149`): "120 fps"; "120 fps, on
  trial until this host has shown it can keep up"; "60 fps (144 fps not granted:
  this host hasn't shown it can sustain that rate yet)"; or, while the rate
  below a refused ask runs as a trial, "120 fps on trial (144 fps not granted:
  …)". A reason word from a newer agent is shown as it came. With no session
  open, the hint reads "Above 60 only where this device has shown it can sustain
  it".
- **"Try N fps again"** (`views/remote/RemoteControl.vue:1780`) appears under
  the picker for an `unproven` refusal, from an agent that advertises
  `fps-retry` (`frameRateCanRetry`, `composables/frameRate.ts:166`). It re-sends
  the pick with `retry: true`, and the agent trials the step-down again from the
  top. ⚠️ It is a text button inside the Video tab. The page's own blue "Try
  again", shown after a session fails, has the same icon and reconnects
  instead.

### A session says what it got (FR-135 R2)

An agent that advertises `media-plan` states its plan on the control channel
(`rc:media-plan`, [`remote-control.md` §5.1h](remote-control.md#51h-a-session-says-what-it-got-fr-135-r2)).
An older agent sends none, and its sessions read as they did. The reading and the
wording are pure, in `composables/mediaPlan.ts`, and `useRemoteControl` holds the
refs: `mediaPlan`, `mediaStop`, `mediaSilent`, `mediaDataArrived`, `viewerHz`.

| On the page | `data-testid` | When | Says |
|---|---|---|---|
| Overlay | `rc-media-stop` | The plan carries `stopped` | "This device could not open a video encoder. It tried h264_nvenc, h264_amf and h264_vulkan.", the device's word, and the backend's own words. The list spans the session's opens, so a failed rebuild lists the cell it ran on, and the words hold for that too |
| Overlay | `rc-media-silent` | A plan, no `stopped`, never painted, 6 s (`nextMediaSilent`) | "This device is set to send H.265 over the data channel, and no frame has arrived." With a decode worker's byte count above zero (`mediaDataArrived`), "Data has arrived, and no picture has decoded from it." instead, because that is not a device that sent nothing. This overlay is what a page has on the two pumps that are not instrumented |
| Stats pill | `rc-media-ladder` | The plan has at least one step down | The last arrow and the count, every arrow on hover |

- **The overlays sit below `rc.mediaUnavailable` and `rc.mediaWaiting`**, because
  those name a cause and a fix, and above "waiting for agent to publish a video
  track", which this replaces when there is more to say
  (`views/remote/RemoteControl.vue`).
- **The words outlive the watchdog's re-create.** `scheduleReconnect` keeps the stop,
  the silence and the plan across its `teardown`, or the page's one sentence would
  blink every 10 s. A new session's first plan knows only the transport and the codec,
  so it leaves the words alone (`mediaPlanAction`). An opened encoder clears them, and
  so does the first picture. For a stop, that means the first frame past the settle
  (`mediaStopAfterTick`): `encoder_fault` only follows frames and they ride another
  channel, so a frame counted just after the stop may have been sent before it. Without
  the wait, that frame took the sentence down and the plan never said it again.
- **The overlays live on the connected stage, so the redial has a stage of its own**
  (`noPictureWhileRedialling`, the same shape as FR-93 P5j's `rc-host-asleep`). It says
  the same words under the same `data-testid`, with the attempt's own phase in its
  caption: a consent prompt on the device still reads as one. Without it, every
  re-create read "Connection lost", which is false because the device answered, and
  from the fourth attempt the dead-air backoff holds that stage for minutes.
- **The backoff stays and stops blaming the network** (`deadAirNotice`). Three frameless
  sessions still slow the retries to minutes, whatever keeps the frames away. A plan
  came over the control channel, so "its network may be blocking the connection" is
  left off for an attempt that heard one. A plan the redial kept from an earlier attempt
  does not count: the path has to be working now.
- **A plan is kept by `epoch`.** The next session's plan starts at 1 again, so the page
  counts apart from the plan it shows and resets on `teardown`.
- **A plan that says `webrtc` while a data-channel path runs stops that path**, so
  `<video>` shows. Nothing is coming on a channel the agent is not writing to.
- **`viewerHz`** comes from the decode worker (`workers/rc-viewer-hz.ts`), and
  `sendViewerHz` sends it only to an agent with the verb, again at each control
  channel `onopen`. The picker does not read it yet: it waits with the agent's cap,
  past FR-135 §7.1's gates.

## A tab that outlives a promote (#3582)

A promote rolls the pods, and every open tab reconnects in place: the `/ws` comes back,
the remote-control page opens its next session by itself, and the tab keeps the build it
loaded. `utils/staleBuild.ts` reloads only a tab whose navigation fails to load a chunk,
and a tab that stays on one page never navigates. One controller's viewer ran a
pre-November build through three promotes, so viewer fixes never reached the people who
keep a remote-control tab open all day. The page now learns of a new build itself and
reloads at a moment that loses nothing.

**Which build.** The build names itself once (`buildIdPlugin`, `ui/buildIdPlugin.ts:43`):
a hash of the names of the files it emits (`buildIdOf`, `buildIdPlugin.ts:36`), each one
content-hashed by Vite, so the id changes whenever the bundle's code, styles or assets do. A
file copied as it is from `ui/public/` and an edit to index.html alone do not change it; the
viewer's code is in neither. The same
value goes into `/version.json` beside the bundle (`buildIdPlugin.ts:54`) and into the
shell as `<meta name="roomler-build">` (`buildIdPlugin.ts:64`). nginx serves the file from
`location /` as a real file, `Cache-Control: no-cache`, security headers kept
(`files/nginx-pod.conf:162-164`); the year-long asset rule does not match `.json`
(`nginx-pod.conf:46`). An image older than this has no such file and answers the SPA shell
(200, `text/html`), which the reader ignores (`readServedBuild`,
`ui/src/utils/buildId.ts:71`). `scripts/public-site-smoke.sh:347` checks all three on every
image.

⚠️ **It is not the git commit, on purpose.** The image's UI stage has none (`GIT_SHA`
reaches only the runtime stage, and `.dockerignore` drops `.git`), and an id that changed
with every commit would reload every open tab after a server-only promote and rename every
chunk with it.

**When the page reads** (`createStaleBuildWatch`, `ui/src/utils/staleBuildWatch.ts:148`):
at every `/ws` (re)connect, which the shell binds (`AppLayout.vue:770`); every 10 minutes
while visible, and on becoming visible after that long; and once more 31 s after a first
mismatch (`armConfirm`, `staleBuildWatch.ts:362`), so the second read does not wait 10
minutes. A confirming read that teaches nothing (a 5xx mid-roll, a captive portal, a
rolled-back image that answers the SPA shell for the file) doubles that wait, from 30 s up to
10 minutes, rather than asking again at once; and reads closer than 5 s fold into one, so a
socket that flaps asks once (`requestRead`, `staleBuildWatch.ts:373`).

```mermaid
flowchart TB
    R["read /version.json<br/>no-store, a fresh ?t="] --> S{"this page's build?"}
    S -->|yes| N["nothing is stale"]
    S -->|"no, a first read"| P["pending: read again in 31 s"]
    P --> R
    S -->|"no, the same id ≥ 30 s later"| C["confirmed"]
    C --> D{"decideStaleBuildAction"}
    D -->|reload| RL["reload, one in 10 min"]
    D -->|"reload-and-resume"| RR["stop the redial, record the resume, reload"]
    D -->|banner| B["the banner; asked again as the page changes"]
    B --> D
```

**What it does** (`decideStaleBuildAction`, `ui/src/utils/staleBuildPolicy.ts:159`):

| The page, once a new build is confirmed | What it does |
|---|---|
| nothing live, nothing typed, no dialog | reloads |
| a remote session already cut and waiting on its redial, on a socket connected for 30 s | reloads, and the next load resumes the session |
| a live remote session, an external one (FR-52), a call or the call page, typed text, an open dialog, a hold | shows "A new version of Roomler is ready" with Reload, and reloads by itself at the next quiet moment |
| typed text or a dialog (never a hold), in a page hidden with no input for 30 minutes | reloads |

A **hold** is what a reload would lose for good, so no amount of hidden, idle time gives it
up: an element marked `data-unsaved` (the machine key `GrowthChannels.vue:222` and the
ephemeral enrollment key `EnrollKeysSection.vue:179`, each shown once), an upload in flight
anywhere on the page (`uploadsInProgress`, `api/client.ts:251`), and a remote session that
ended within 5 minutes, or whose rating prompt is still up (`RC_ENDED_HOLD_MS`,
`useRemoteControl.ts:465`). The call page counts as a call whatever the store says
(`AppLayout.vue:772`): it leaves and rejoins on a pod roll and stays after the call ends, and a
reload there asks "Reload site?".

A quiet moment is the session or the call ending, the text sent, the dialog closed, a hold
gone, or, for a remote session, its next cut on a settled socket. The banner's Reload resumes
a remote session, and where the device asks someone to allow each session (`consent_asks`),
it says the device asks again; for an external session it says the device's password is
needed again, and for a call that reloading leaves it (`bannerHint`,
`staleBuildPolicy.ts:185`; `components/layout/NewBuildBanner.vue`, rendered by `App.vue:19`
inside a fullscreen viewer too).

**The guards**, each one a way the page could be left worse than before:

| Guard | Why | Where |
|---|---|---|
| a mismatch reads the same twice, 30 s apart | during a roll one pod serves the old build and one the new | `trackServedBuild`, `staleBuildPolicy.ts:69` |
| this page's own `/ws` connected for 30 s (down, or retrying, is never steady) | a roll that just cut it may still serve both builds, and a reload then can take its shell from one and its chunks from the other: a blank page | `settled`, `staleBuildWatch.ts:224` |
| one automatic reload in 10 minutes, claimed in sessionStorage | never a loop. Where storage cannot be written, no automatic reload at all: the banner only | `NEW_BUILD_RELOAD_KEY`, `staleBuildPolicy.ts:47`; `claimReload` in `utils/staleBuild.ts` |
| offline, nothing | a reload would trade the app for the browser's error page | `readNow`, `staleBuildWatch.ts:386` |
| the person's hands | a plain reload waits until no key, click or wheel for 5 s | `BUSY_MS`, `staleBuildWatch.ts:72` |

**The resume.** A remote session that a cut put on its redial (`scheduleReconnect`) is handed
over: its pending retry is stopped (`holdRedial`, `useRemoteControl.ts:9647`), the args its
redial would have used are written to sessionStorage with the time, the path and the
signed-in person (`writeRcResumeIntent`, `staleBuildPolicy.ts:242`), and the page reloads. A
reload that has not happened 20 s later gives the redial back and deletes the record
(`startReload`, `staleBuildWatch.ts:284`; `clearRcResumeIntent`, `staleBuildPolicy.ts:265`),
or an F5 inside the minute would open the session a second time beside the redial. The next
load takes the record out of storage before any page renders (`installStaleBuildWatch`,
`staleBuildWatch.ts:515`, called before the router at `main.ts:46`). The viewer claims it once
it has its device and the person (`resumeAfterNewBuild`, `RemoteControl.vue:5500`): once,
under a minute old, on the first page that load rendered, for the same person, device,
organization and path (`resumeArgsFor`, `staleBuildPolicy.ts:311`). It resumes through the
redial's own ladder (`resumeAfterReload`, `useRemoteControl.ts:9672`), never as a first
connect: a first connect starts the ladder at 0, and `rc:error` retries `agent_offline` only
past it, so a tab that reloaded while its device restarted for an update would have stopped
there, where the redial keeps trying. Its record is `after_drop`, out of FR-22's
first-attempt numbers ([remote-control.md §11.15](remote-control.md#1115-a-new-build-takes-over-a-redial-3582)).

- ⚠️ **Only on the redial path.** A request or a consent prompt in flight reads `live`
  (`rcReloadState`, `useRemoteControl.ts:440`). A reload then would abandon a question the
  host may be reading, and the resume would ask it again. On the redial path the device
  decides consent as it would have for the redial, so a resume asks the host nothing more.
- ⚠️ **Never an external session.** Its login opened one session, and another costs the
  device's password (FR-52). It never reads `redialing`, and no resume is written for it.
- ⚠️ **Not at the roll's own cut.** That cut is when the page's socket is not settled, and
  the page's redial has the session back within seconds, long before a second read 30 s
  later could confirm anything. So a session the roll cut comes back as it always did, the
  banner shows, and the reload happens at the session's next cut on a settled socket (the
  device restarting for an update, a network change on either end), at its end, or at a
  click on Reload.
- A file transfer the redial would resume (rc.19) and a recording's download keep the
  redial too: they live in this page's memory only.

**The instrument.** Every `rc.connect` record names the page's build (`fields.build`,
`rcConnectTimingUpload.ts:182`; the record is `v` 2), so a field read can tell a stale
viewer from a bug directly
([remote-control.md §12.1.5](remote-control.md#1215-persisted-marks-part-4--where-a-stalls-phase-outlives-the-tab)).

Tests: `staleBuildPolicy.spec.ts` (the table, the guards, the holds, the record),
`staleBuildWatch.spec.ts` (the reads and their backoff, the reload, the hand-over and the
claim, on fake timers), `rcResumeAfterReload.spec.ts` (the resume on the real composable:
on the ladder, held, given back), `buildId.spec.ts` (the plugin's file and shell agree),
`rcReloadState.spec.ts`, and `api/client.spec.ts` (uploads in flight).

## Observability components

`ui/src/components/stats/`: `MeshGraph` (d3 force graph of overlay edges +
carrier types), `TimeSeriesChart`, `UptimeStrip`, `UsagePanel`, `UsageTable`,
`UsageTimeline`, `RangePicker` — fed by the `stats` store from
`/api/tenant/{tid}/stats/*` and `/api/admin/stats/*`.

## Admin components (`ui/src/components/admin/`)

`AgentsSection` (device management: caps chips, exec policy, join-org, update) ·
`AgentCrashesDialog` · `AgentLogsDialog` · `DeviceConsoleDialog` (fleet-RPC exec) ·
`ExecAuditSection` · `ExecPolicyDialog` · `AclSection` + `OverlayAclSection`
(overlay L3 ACL + mode) · `OverlaySubnetRoutesSection` (approved routes / exit
nodes) · `MagicDnsSection` · `TunnelPoliciesSection` · `MembersSection` ·
`RolesSection` · `SettingsSection`.

### The devices list's "needs a person" chips

`AgentsSection` shows these in both its table and its cards. Each appears only when someone has to act, and
stays silent when the data is missing:

| chip | when | rule |
|---|---|---|
| `credential expires in N d` · `credential expired` | the agent token is within 60 days of expiry, so it is not renewing (FR-103) | `ui/src/utils/credentialExpiry.ts` |
| `not updating` | the device is online, and the **oldest** release it has not installed has been out for 24 h (FR-106). The tooltip names that release and the remedy for the OS. On Linux, a per-user install made before 0.4.138 needs one install by a person. A failed release fetch, or a version that does not parse, shows nothing | `ui/src/utils/updateStall.ts`; the list comes from `useAgentStore().fetchReleases()` (`/api/agent/latest-release`, cached 30 min) |

⚠️ The age is the oldest missed release's, not the newest's. Releases can come 12 h apart: measured from the
newest, a device stalled for weeks would lose its chip at every release.

### The Devices page at every width (FR-116 P1)

One feed, one menu, one set of policy controls; only the presentation changes with the width.

```mermaid
flowchart LR
    F["deviceStore.items<br/>GET …/device (search · kind · page)"] --> W{"width"}
    W -->|"≥ 960 px"| G["the grid<br/>(v-data-table-server)"]
    W -->|"&lt; 960 px"| C["cards: 2 columns to 960 px;<br/>&lt; 600 px one line each,<br/>the card on a tap (FR-120 P4)"]
    G & C --> M["DeviceActionsMenu"] -->|"action key"| D["onDeviceAction → the page's dialogs"]
    G & C --> P["DeviceAccessPolicyFields"] -->|"one PUT per change"| A["…/access-policy"]
```

| piece | role |
|---|---|
| `components/admin/DeviceActionsMenu.vue` | the only action menu for a device row. It emits an action key and opens nothing itself, so the grid and the cards cannot offer different things (the cards' own copy had lost "Edit name & tags", and its overlay links the tenant id) |
| `components/admin/DeviceAccessPolicyFields.vue` | the six access-policy controls: `variant="plain"` in the grid's Consent cell, `outlined` and labelled in a card's **Access** panel, where the hints that live in `title=` (never shown on touch) are spelled out on demand |
| `components/admin/devicePolicy.ts` | the choices, the hints, and `policySummary()` — the collapsed **Access** panel's one line: the consent mode always, then only what differs from the defaults |
| a card | the device's identity (`deviceLabel()`, then the machine name unless hidden, OS, version, last seen), its status, every "needs a person" chip, its tags; **Access** and **Details** (overlay addresses, MagicDNS, keys, ids, codecs) open on demand; Connect and the menu. **On a phone** (&lt; 600 px) it starts as one line, the name, the OS, the status chip and one action (Connect, or a tunnel client's menu), and its head, a `stack-toggle` button, opens the rest (`isCardOpen`, FR-120 P4, D2); a tablet never sees the closed line. Ten open cards were 3.7× the desktop grid at 360 px |
| `components/admin/EnrollKeysSection.vue` | below the cards: the ephemeral enrollment keys. On a phone one line a key (its label, its state, Revoke), opened on a tap for its uses, expiry, device deadline and mint date, 20 at a time (FR-120 P4); with every device folded, its six labelled blocks a key still held the page at 2.5× |

Tunnel clients are rows of the same feed (the kind filter), never a separate table. The grid did not change:
master and P1 screenshot to the same pixels at 1300 and 2400 px.

⚠️ **A device is named by `deviceLabel()`** (`composables/useHideDeviceName.ts`): its display name when one is
set, else the machine name — in every list, card, dialog title and notice a person reads. The technical `name`
stays wherever a machine reads it back: a MagicDNS label, an `overlay_exit_node = "…"` line, a CLI selector.

## Calls: a black tile says why (FR-124)

A call tile that shows black says why, on the screen of the person who can fix it
([FR-124](fr/FR-124-black-call-tile-says-why.md), #2945). Two causes looked identical before, a black square
with no reason given:

| cause | what the tile does | where else it shows |
|---|---|---|
| **The camera sends a black picture**: a covered lens, a closed privacy shutter, a camera-off key that blanks the sensor while the device stays attached | a notice over the picture, in text. Your own tile: *"Your camera is sending a black picture · Is the lens covered?"*. A peer's: *"Black picture from this camera · Its lens may be covered"*. A compact tile: *"Black picture"* | for YOUR camera only, a closable warning over the call (`call.blackPicture`): you are the one who can uncover it |
| **The browser refused to start the tile** (its autoplay policy: audible playback before the page has had a gesture) | the picture plays muted at once, with *"Tap for sound"*; if even muted playback is refused, *"Tap to play"* | — the next click or key anywhere on the page brings the sound back |

Found on 2026-10-26: a covered webcam on neo16 was reported as the *MacBook's* camera not working. Both devices
were signed in to one account, so both tiles carried the same name, and nothing on screen said which picture was
black. Every layer under it was healthy. The hour that took is in the FR's §2.1.

### The black-picture detector

```mermaid
flowchart TB
    T["every second, per tile<br/>(watchBlackPicture)"] --> H{"page hidden?"}
    H -->|yes| KEEP["skip — the verdict stays"]
    H -->|no| A{"judgeable?<br/>a camera track that is enabled, live and receiving,<br/>no FR-30 camera-off, not a screen share"}
    A -->|no| RESET["no verdict — the run starts again,<br/>a black verdict clears"]
    A -->|yes| S["draw the &lt;video&gt; into 16×12,<br/>mean and peak luma (0–255)"]
    S --> B{"mean ≤ 4 and peak ≤ 16?"}
    B -->|no| CLEAR["a picture — not black"]
    B -->|yes| RUN{"3 in a row?"}
    RUN -->|not yet| WAIT["keep counting"]
    RUN -->|yes| BLACK["black: the tile's notice,<br/>emit picture-black(true)"]
    BLACK --> L["layout → ConferenceView:<br/>count per stream key"]
    L --> W{"the 'local' key?"}
    W -->|yes| WARN["the call page's warning<br/>(closable for this black spell)"]
```

- **The bar is "no light at all", not "dim".** A covered lens measured mean luma 0 and peak 1–3 over the 16×12
  sample. A dim room under auto-exposure reads far brighter, and a dark screen with text has bright peaks.
  `utils/blackPicture.ts:22-28` holds the three numbers. `blackPicture.spec.ts` pins each of them with a case on
  either side.
- **The samples never leave the tab**, and nothing goes on the wire. The sender judges its own picture and each
  viewer judges what it decodes, so an older client changes nothing for anyone.
- ⚠️ **A camera that is OFF is not a covered camera.** A disabled track (your own camera turned off) still paints
  black at full size, so the tile's own `hasVideoTrack` stays true. Only `cameraTrackLive()`
  (`VideoTile.vue:446`) stands between "camera off" and "lens covered": it requires the track to be `enabled`,
  `live` and not `muted`. A peer's own word that their camera is off (FR-30's `videoPaused`) also wins: the avatar
  shows, never the notice. A shared screen is never judged, because a black slide is not a lens.
- ⚠️ **A count per stream key, not a flag** (`composables/useBlackPictureTiles.ts:15`). Alone in a call with a
  floating self-view you are on two tiles at once. When someone joins, the spotlight tile unmounts and reports
  `false` while the floating one is still black, so a flag would drop your warning there. Each tile reports
  changes only, and `false` on unmount if it was black (`VideoTile.vue:466`), so the count stays honest.

### A refused start

```mermaid
sequenceDiagram
    participant T as VideoTile
    participant B as browser
    participant P as the person
    T->>B: play() (unmuted: a peer's tile carries their audio)
    B-->>T: NotAllowedError (no gesture on the page yet)
    T->>T: soundBlocked = true → :muted binds true
    T->>B: play() again, muted
    B-->>T: playing — the picture is on screen
    T->>P: "Tap for sound" on the tile
    P->>B: a click or a key anywhere on the page
    Note over T: capture-phase listener, armed once
    T->>T: soundBlocked = false → unmuted
    T->>B: play() inside the gesture
    B-->>T: playing with sound
```

- `startPlayback()` (`VideoTile.vue:317`) replaces the two `play().catch(() => {})` calls that swallowed the
  refusal. Before FR-124, a refused tile stayed black until its stream changed.
- `AbortError` (a newer `srcObject` or `play()` took over) is not a refusal and is ignored. Anything else is
  logged with `console.warn` and never hidden.
- ⚠️ **`:muted="isLocal || soundBlocked"`** (`VideoTile.vue:29`): a binding, not just an assignment, so a
  re-render can't unmute the element behind the browser's back.
- ⚠️ **`click`, not `pointerdown`** (`armGestureRetry`, `VideoTile.vue:368`): on a touch screen the activation
  comes with the pointer's release, so a `play()` at `pointerdown` would be refused again. The listener sits in the
  capture phase, so a click that a control stops on its way still counts, and it is removed once used and on
  unmount.

Not covered by FR-124: `MiniConference.vue`'s own `<video>` elements. They never call `play()`, and the mini view
is reached only after a Join click. A related gap, read from the code and left for its own fix: remote audio plays
only through a rendered tile, so a participant whose tile is hidden or past the tiled maximum is not heard.

## Everything else

- **Chat**: TipTap v3 editor (markdown, mentions, emoji via emoji-mart, Giphy
  picker), threads, reactions, pins.
- **Conference**: mediasoup-client composables + `VideoTile`, layout modes,
  active-speaker tracking, PiP. A black tile says why (above).
- **Theming**: Vuetify light/dark with localStorage persistence.
- **API client**: `ui/src/api/client.ts` — token injection + refresh handling.
- **Dev proxy**: Vite proxies `/api` + `/ws` to `http://localhost:5001`.
- ⚠️ **The ws store holds ONE handler per frame type** (`onMediaMessage` is a `Map.set`,
  `stores/ws.ts:432`): a second registration of a type takes the first's frames away. Conference
  registers its `media:*` types through it (`stores/conference.ts:146-151`).

## Conventions

**Plugin order** (`main.ts`): i18n → vuetify → pinia → router.

**A view that owns per-subject state remounts when its subject changes.** Vue Router
reuses the mounted component when only a param changes, so a view that resolves its
subject once in `onMounted` kept showing the previous subject under the new URL (#1631:
the remote viewer stayed on device A after the nav picked B, and Connect dialled A).
The fix is route meta, not per-view watchers: `meta.remountOn: '<param>'`
(`plugins/router.ts`), read by `components/layout/KeyedRouterView.vue`, keys the view by
that one param. Every other route stays unkeyed on purpose — several views rewrite
`route.query` in place and must not remount — so never key by `fullPath`.

**The call page remounts by room, and asks before it leaves a call (#1637).** The app
bar's Active Calls menu and the "Call started in …" snackbar push another room's call
onto the call page in place. Until `room-call` carried `remountOn: 'roomId'`
(`plugins/router.ts:242`), the page in room A's call kept A's call, chat and name under
room B's URL: a message typed there went to B, and Leave left B, a call it was never in,
while the server still listed the person in A. A remount alone would leave A's call
running with nothing on the page to say so, because the mini-call widget hides on every
call page. So the page settles its own call first, in `onBeforeRouteUpdate`
(`views/conference/ConferenceView.vue:693`), with the question the Join button asks:

| The page, when another room's call is pushed onto it | What happens |
|---|---|
| in its room's call, **Switch Call** | it leaves that call (`leaveThisCall`, `:646`), then the page for the other room joins that room's call |
| in its room's call, **Cancel**, Escape or a click outside | the navigation is refused. The call and the URL stay where they were |
| in no call here | it remounts for the other room with no question, and joins nothing |

The join crosses the remount as a one-shot hand-off (`composables/callSwitch.ts:22`,
taken at `ConferenceView.vue:776`). It is for that one room, spent by the next mount
whatever its room, and void after 10 s. It is never a query or `history.state`, so a reload
or a back-and-forward cannot put a camera into a call nobody just asked to join.
`ConferenceViewRoomSwitch.spec.ts` runs the real page against the real route record.
`e2e/conference-room-switch.spec.ts` drives the three rows in a browser against a real API
and reads the outcome from the server's participant list for each room. Its join waits for
the page's socket, as a person's does. Since #3416 the page waits too: a join sent before
the socket had its connection id opened a call session with none, which a leave from that
page, naming its connection, did not close
([real-time.md](real-time.md#the-call-session-join-leave-socket-close-sweep-3416)).

**A link clicked inside the app never takes a tab away from a live session (#2841).** A
page that holds one carries `meta.liveSession`: both remote viewers (`agent-remote`,
`connect-session`) and the call (`room-call`). `openInAppLink` (`utils/inAppLink.ts`) opens
a link from such a page in a new tab. Anywhere else it navigates in place, and a link to
the page already showing does nothing. The bell (`components/layout/NotificationPanel.vue`)
goes through it. It is the in-app half of #2827's rule for a push card's click. Measured on
2026-10-24: a consent request clicked in the bell moved a live viewer to the consent page,
and the device hung that session up 3 s later. ⚠️ A new view that owns a live session needs
the flag, or the bell will navigate it away. `inAppLink.spec.ts` locks the three records.

**Stores** use the Pinia setup pattern: `defineStore('name', () => { … })`.

**Vuetify** runs light + dark themes with auto-import tree-shaking via
`vite-plugin-vuetify`.

### Responsive page padding

| Surface | Class |
|---|---|
| Top-level view | `<v-container fluid class="pa-2 pa-md-4 pa-xl-6">` — 8 px mobile / 16 px tablet+ / 24 px ≥1920 px |
| Empty-state block | `pa-4 pa-md-6 pa-lg-8` |
| Heading | `text-h5 text-md-h4` — shrinks one step on phone |
| Section CTA | `size="large"` — **not** `x-large`, which overflows narrow viewports |
| Marketing / legal (`LandingView`, `Terms`, `Privacy`) | `py-6 py-md-12` / `py-8 py-md-16` in place of fixed `py-12` / `py-16` |

Hide secondary toolbar items below `sm` with `d-none d-sm-inline-flex`, and
surface a phone fallback alongside.

### Phones and tablets (FR-116)

Three widths matter, and each has its own job. Vuetify's own `mobile` means **narrower than 1280 px**
here (`plugins/vuetify.ts` sets no `mobileBreakpoint`), so it is the drawer's switch and nothing else's:

```mermaid
flowchart LR
    XS["xs · &lt; 600 px<br/>phone: full-screen dialogs,<br/>icon-only bar indicators,<br/>rows that open on a tap,<br/>a page ≤ 2× its desktop height"] --> SM["sm · 600–959 px<br/>portrait tablet: card layouts<br/>(two columns)"]
    SM --> MD["md · 960–1279 px<br/>tables and grids;<br/>the drawer still overlays"]
    MD --> LG["lg · ≥ 1280 px<br/>the permanent drawer"]
    T["a touch screen, any width<br/>(pointer: coarse) · 36 px hit areas<br/>(hover: none) · a tap for every hover"]
    T -.-> XS
    T -.-> SM
    T -.-> MD
    T -.-> LG
```

FR-120 added four rules to FR-116's, each in the table below: **a disabled control says why, as text**; **nothing
is hover-only**, facts included; **a touch target is at least 36 px**; and **a page at 360 px is at most twice
its desktop height** for the same data.

| rule | where | why |
|---|---|---|
| **The page never scrolls sideways.** `v-main` clips horizontal overflow; a wide table scrolls inside its own box. A page outside AppLayout clips its own gutters with `.page-no-sideways` (`overflow-x: clip`, never `hidden`, which would make the page a second vertical scroller) | `AppLayout.vue` `.app-main-no-scroll`; `styles/responsive.css` `.page-no-sideways` on sign-in, sign-up, the OAuth callback, the invite landing, the newsletter pages and Not Found (FR-120 P5) | the house `pa-2` (8 px) is narrower than a `v-row`'s 12 px gutters, so every page with a row swayed 4 px under a finger — sign-in and sign-up still did, at 360 and 390 px, until FR-120 |
| **A new page opens at its top.** A path change scrolls `v-main` to 0; a query or hash change keeps the place. A view that moves the reader within itself scrolls the target into view (`el.scrollIntoView`, with a `scroll-margin-top` clearing the app bar), never the window | `AppLayout.vue`, the `route.path` watcher; `TutorialView.vue` `go()` (FR-120 P5) | `v-main` is the scroller, not the window, so `window.scrollTo` and a router `scrollBehavior` do nothing: the tutorial's chapter navigation changed the content off-screen on a phone |
| **The bar's phone budget** (&lt; 600 px): ☰ · title · your call · other calls · unread · search · bell · avatar. The tutorial `?` and the theme toggle move into the avatar menu, and the call and unread indicators become icons with a badge | `AppLayout.vue` `<v-app-bar>` | past ~344 px the avatar menu — and Logout with it — was pushed off-screen |
| **What the drawer signals reaches the bar** when the drawer is closed: a dot on ☰ for another org's activity, and your call's controls (Return · Mute · Camera · Hang up) as a menu off the call page | `AppLayout.vue`; `composables/useCallControls.ts`, shared with `MiniConference` | the drawer is temporary below 1280 px, so a call kept a live mic with nothing on screen saying so |
| **A dialog of any substance is full screen below 600 px** and carries its own close control. Its title never clips silently: a label then a name ("Agent logs — …") may shrink with an ellipsis to keep its buttons on screen, but **a question to confirm wraps**, because the name in it is what the reader confirms | `composables/useResponsiveDialog.ts` · the Devices page's evict, update-all, delete, add-to-an-org and routes dialogs, and its rotate-key question (`AgentsSection.vue`) · a member's roles · the viewer's agent log (its line count rides a second toolbar row on a phone) · GIFs | a centred dialog leaves 264 px of content at 360 px; a full-screen one has no scrim to tap. At 360 px the evict question was cut off before the device's name began, and the rotate-key one partway through it (FR-120 P3, measured); by its toolbar's own sizes the agent log's select and three buttons filled its 312 px dialog, leaving the title 0 px |
| **A disabled control says why, as text** — a caption under it, or a list item's subtitle, never only its `title`. A toolbar icon that has no room for a caption says why on a tap: it sits in a span that takes the tap a disabled button does not, and the reason shows in the snackbar | the viewer's Settings toggles (each `rc-*-why` caption reads the reason its title holds) · the viewer's toolbar: clipboard, Record and Privacy, refused or not ready (`explainRefusal`, `RemoteControl.vue`) · its touch-mode control (a tap says how the new mode works) · a failed transfer's error | a disabled `v-btn` takes no pointer events, so its `title` reached **no one**, on any device; the touch-mode help, offered only to a finger, was a `title` a finger never sees (FR-120 P1b) |
| **A plain `v-table` with actions or results on its right is a card per row below 600 px**: `class="table-stack-xs"` on the table and `data-label="…"` on each cell. One template for both widths; above 600 px nothing changes. A `v-data-table` uses Vuetify's own mobile rows instead (next row) | `styles/responsive.css` (imported in `main.ts`); the ACL lists, Overlay routes, the audit lists, Roles, enrollment keys, the usage panel's Calls and Tunnel sessions, the keep-busy history (FR-120 P4b) | the rightmost columns — Save, Edit/Delete, Result, the command — sat off-screen behind a sideways scroll nobody finds |
| **A list read row after row is compact on a phone, a screenful at a time**: `table-stack-xs--compact` puts a short cell's label beside its value (`stack-block` keeps a long cell's label above it), `usePhoneSlice()` shows 20 rows and then **Show N more**, and each card shows what decides and opens on a tap for the rest (next row but one): an audit card is the device, when and the result, plus an exec command's first two lines | `ExecAuditSection.vue`, `SshAuditSection.vue` (FR-116 P3d, FR-120 P4), `SshActivitySection.vue` (FR-116 P3d) | 100 audit rows as full cards were 43,398 px at 360 px on production; one script alone took 1,936 px. FR-116 made them 20 cards of ~205–270 px, still 3.3–3.6× the desktop's height; FR-120's summary cards are 68–128 px, and the audits measure 1.6–1.7× |
| **A `v-data-table(-server)` takes `:mobile="xs"`**, and any natural-width rule on it (`width: max-content`, `white-space: nowrap`) is scoped to `:not(.<table>--stacked)`, with `overflow-wrap: anywhere` on a stacked value. **On a phone its row is one line**, through the table's `#item` slot (`<template v-if="xs" #item>`): what decides and at most one action, the rest on a tap. The row is `<tr class="v-data-table__tr">` with one `<td class="v-data-table__td stack-row">`, never `v-data-table__tr--mobile`, whose cells are a label \| value grid | `MembersSection.vue` (a name and roles), `InviteManageView.vue` (who for, the state, Copy link), `FilesBrowser.vue` (the name, the size, Download) (FR-120 P4b) | with the desktop rules left on, every stacked value sat past the right edge — the rows showed their titles and nothing else — and one unbroken email widened a row past the screen. Every column stacked made a row ~250–290 px: with 20 rows, Files was 4.65× its desktop height, Invites 4.72× and Members 3.81×; one line a row brings them to 1.28–1.36× (FR-120 P4b, the same rows served to both builds). ⚠️ In `v-data-table__tr--mobile` the line was squeezed into half the width ("An/yo/ne") and the opened details sat beside it |
| **A page of independent settings is one expansion panel per setting below 600 px, titled with its current state** ("On", "Allowed", "Files: ask · Audio: off"); an unknown state shows nothing, never a guess | `components/admin/SettingsGate.vue`, used by `SettingsSection.vue` | Settings was ~4 screens of warnings and captions before its last switch; now one screen of decisions, each opening on demand. From 600 px up the gate renders the card it always was |
| **Nothing is hover-only on a touch screen** (`(hover: none)`): what a tooltip or a hover reveals is reachable by a tap, or shown as a persistent hint — and a control hidden until hover takes no taps while hidden (`pointer-events: none`) | `RolesSection.vue` (grants on tap; descriptions as hints) · `MessageBubble.vue` (a ⋮ menu) · `VideoTile.vue` (a tap shows the tile's actions) · the viewer's files drawer (a › into a folder, "Upload here") · **a chip that explains itself is a `FactChip`** (`components/common/FactChip.vue`: its meaning in a small menu on a tap, its `title` kept for a mouse) — every titled chip on the Devices page, unless its tap already opens what explains it · **a chart reads a datum on a tap**: the uptime strip and the viewing windows report the period under a tap (or a mouse pointing, or the arrow keys) in a line below them, and the mesh graph gives each carrier a 14 px hit line whose tap pins its carrier and round trip | a permission's description was a tooltip on its label, so the tap meant to read it toggled the permission; a message's invisible hover buttons pinned it for the whole room; a tap on a face in a call hit an invisible picture-in-picture button. At 360 px the Devices cards held 10 dots and 19 chips that explained themselves only by hover, and Analytics 498 uptime segments (FR-120 P1a, measured) |
| **On a touch screen a target is at least 36 × 36 px — a hit area, not a size.** Under `(pointer: coarse)` every `v-btn` and every tappable chip (`.v-chip--link`) gets an invisible `::before` grown to 36 px, centred, inside its own stacking context (`isolation: isolate`) and under its own content, so a chip's close button keeps its taps; a tappable chip clips its label instead of itself. A chip group, which scrolls sideways and so clips vertically, gives its chips the room their hit areas need | `styles/responsive.css` · `MeshGraph.vue` (the carrier filter's chips, 10 px above and below on a touch screen) (FR-120 P6) | `size="small"` buttons are 28 px tall and x-small chips 20 px: at 360 px the rooms list had 43 targets under 36 px and Devices 32 (FR-120 P6, measured on production). A mouse matches none of it, so a desktop is pixel-identical. ⚠️ A hit area cannot leave a box that clips (`overflow: hidden`, a scroller): give the target room inside it |
| **On a phone, show what decides and open the rest on a tap — and a page at 360 px is at most twice its desktop height for the same data**: a row shows its name, its state, its total, and at most one action; access, details, grants and breakdowns open on demand. `useOpenRows()` holds which rows are open; the row's head is a `stack-toggle` button the width of the line, and in a `table-stack-xs` table a `stack-detail` cell shows only while its row is `stack-open` | `composables/useOpenRows.ts`, `styles/responsive.css` · the Devices cards (one line each until tapped, `AgentsSection.vue`) and the enrollment keys under them · the audits' cards · Billing's plans (users, devices and sessions at a time; the other six limits on a tap) · Members, Invites and Files (one line a row; the `v-data-table` row above) · the agent log (its newest 200 lines, then 200 more: `usePhoneSlice`) · the rooms list (a search lists the matching rooms, each with its path) · the Devices cards' Access / Details panels · `SettingsGate.vue` · `UsageTable.vue` (User · Total; a row opens the breakdown) · a role's grants | the Devices page was ~11,900 px of open selects and checkboxes at 390 px; Settings was ~3,600 px before its last switch. At 360 px on production, Devices was still 3.66× its desktop height, the audits 3.3–3.6× and Billing 2.24×; FR-120 P4 brought them to 1.26×, 1.6–1.7× and 1.52× (measured) |
| **A device is named by its display name everywhere a person reads one**: `deviceLabel()` (`composables/useHideDeviceName.ts`) and `useNodeLabel()` on the client, and the server sends the friendly name beside the machine's (`display_name`, `agent_display_name`; additive, `null` when none) | the Devices page and its dialogs, the audit and overlay pages, the Tunnel ACL, `crates/api/src/routes/stats.rs` (uptime) and `crates/api/src/routes/usage.rs` (D4) | the device list said "Reception PC" while the dialogs, the audit lists, the uptime strip, the usage timeline and each agent session said `DESKTOP-…` |
| **A side panel that would squeeze the main view overlays it below 960 px**, one at a time, with a close button | `ConferenceView.vue` (chat, files) | a 320 px chat column left the call ~70 px wide and cut off the pre-join pickers; the chat now starts closed there, with a dot on its button when someone writes |
| **A drawer that can cover a phone takes the full width and carries its own close button** | `GrowthView.vue` (org timeline), the viewer's files drawer | a 440 px temporary drawer on a 390 px screen leaves no scrim to tap — no way out |
| **A tab window over wide content takes `:touch="false"`; a tab row that can overflow takes `show-arrows`** | `GrowthView.vue`, `AnalyticsView.vue`, the viewer's Settings tabs | a sideways swipe across a wide table switched the **tab**; and below 1280 px Vuetify hides the arrows unless asked, so tabs past the edge gave no sign of themselves |
| **A number or caption that must be read never sits at the end of a card title** on a phone — it moves into the card's text | `GrowthView.vue` (the weekly-active-orgs north star) | a title never wraps: at 360 px the caption pushed the number off the card |
| **While a session runs, the remote viewer's toolbar holds only Back, the device's name, Disconnect and Fullscreen**, at every width; the session's state chips and every tool ride their own row under it (`#rc-tb-strip`, one `<Teleport>`: one instance of each control, moved, never copied), state first, scrolling sideways; from 960 px the stats readout comes last, in whole pills only, so it never pushes a tool out of view. Connect's phases keep the one row (below 960 px its items in flex `order`). The stage's alerts overlay the stage | `RemoteControl.vue` (`rc-tb-*`, `sessionToolsOnStrip`, `.rc-stage-alerts`) | the title went to 0 px and Disconnect started off-screen; with every tool still in that one row, the row overflowed and the name was held at its 96 px minimum on a phone ("FR-…" at 360 px) and at 0 px on a 1300 px desktop, with the state chips past the edge or squeezed to stubs (FR-116 P5c); each alert's coming and going resized the frame, re-sent `rc:resolution` and rebuilt the agent's encoder |
| **On a phone held sideways, a running session takes the whole screen** (FR-125): with a coarse pointer, a page wider than tall and under 500 px tall (`isPhoneSideways`, `composables/useImmersiveViewer.ts:31`), the app bar hides (`AppLayout.vue:414`), the viewer's toolbar and tools strip become an overlay out of view (still mounted: the strip is the session tools' Teleport target), and the stage card loses its margin, border and corners. A pull-tab at the stage's top edge slides the bars back down **over** the stage; they hide again after 4 s without a touch, or on a tap on the stage. A per-browser setting in Settings → Display turns the mode off | `useImmersiveViewer.ts`, `RemoteControl.vue` (`immersive`, `:2781`; the pull-tab, `:949`; `.rc-immersive`, `:5897`) | the bars took 149 of a 340–412 px page, leaving the remote desktop 146–254 px, and every built-in touch preset ran off the stage's top (measured on production at six sizes, FR-125 §2) |
| **Text that cannot break still wraps** (`overflow-wrap: anywhere`) | message content (`MessageBubble.vue`), stacked table values | one long URL or email widened a whole list past the screen |
| **Sign-in fields say what they are**: `autocapitalize="none"`, `autocorrect="off"`, `spellcheck="false"` on the username, `autocomplete` on every credential | `LoginView.vue`, `RegisterView.vue` | iOS capitalised the username, and the server matches it exactly |
| **The public landing page's sections are a ☰ menu below 960 px**, and its wordmark never shrinks (`flex-0-0` on the title) | `LandingView.vue` | the links were hidden below 600 px with no other way to them, and the wordmark was clipped to "Roo" |
| **A toolbar's search takes a row of its own below 600 px** (`order-*` and `flex: 1 1 100%`); the buttons take the next | Members, Invites, Files | beside the title and the buttons it was left ~100 px ("Searc") |
| **A title row with buttons wraps** (`flex-wrap ga-2`), and its caption is a `v-card-subtitle class="text-wrap"`, never text inside the title | e.g. `OverlayAclSection.vue` | `v-card-title` and `v-card-subtitle` never wrap: Overlay ACL's caption pushed **New rule** off the card below ~800 px |
| **A row of form fields wraps**: `flex-wrap` on the row and a `flex: 1 1 <basis>` on each field | the ACL editors | without wrap a Vuetify field shrinks instead — the Tunnel ACL host field rendered 0 px wide |
| **An overlay node, or an audit row's device, is named by `useNodeLabel()`** — joined on the wire's `agent_id` / `tunnel_client_id`, never on the node's name | `composables/useNodeLabel.ts` | `OverlayNode.name` is a lossy DNS label, and audit rows carry the stored machine name: the device a person had just tapped on Devices came back under another name |

The remote viewer on a phone held sideways (FR-125) moves between three states. Revealing the bars
never resizes the stage, because a resized stage re-sends `rc:resolution`:

```mermaid
stateDiagram-v2
    [*] --> Normal
    Normal --> Immersive: session connected + coarse pointer + sideways + page under 500 px + setting on
    Immersive --> Normal: held upright · session ends · setting off
    Immersive --> Revealed: tap the pull-tab
    Revealed --> Immersive: 4 s without a touch · tap on the stage
    Revealed --> Normal: held upright · session ends
    note right of Revealed
        toolbar + tools strip slide down OVER the stage,
        which keeps its size
    end note
```

A list or a table on a phone goes through one decision:

```mermaid
flowchart TD
    T["a list or table at 360 px"] --> Q1{"every row's actions<br/>and result on screen?"}
    Q1 -- yes --> KEEP["keep it"]
    Q1 -- no --> Q2{"a v-data-table?"}
    Q2 -- yes --> MOB[":mobile=&quot;xs&quot;, and scope its<br/>natural-width rules away"]
    Q2 -- no --> STACK["table-stack-xs +<br/>data-label on each cell"]
    STACK --> Q4{"read row after row,<br/>dozens of rows?"}
    Q4 -- yes --> COMPACT["table-stack-xs--compact +<br/>usePhoneSlice: 20 at a time"]
    Q4 -- no --> Q3
    COMPACT --> Q3
    MOB --> Q3{"over twice the desktop's<br/>height, or a row longer<br/>than a screen?"}
    Q3 -- no --> DONE["done"]
    Q3 -- yes --> DEMAND["a phone row: what decides<br/>and one action; the rest<br/>on a tap (useOpenRows)"]
```

⚠️ **A phone-only line never sits between a `v-if` and its `v-else`.** Vue pairs
`v-else` with the nearest `v-if` sibling. P3a put the refusal's reason line (a
`d-sm-none` element with its own `v-if`) between the SSH audit's refusal chip and
its "granted" chip, so a refusal without a message read **granted** as well, on
every width. Wrap the branch and its phone-only extras in one `<template v-if>`
(fixed in P3d; `auditPhoneLists.spec.ts` fails on the old template).

⚠️ **Prove "the desktop is unchanged" with pixels, against the branch's own
base served beside it** — and grep each dev server's served module for a
marker only its branch has before any screenshot: a dev server that lost its
port to another one dies silently, and a before/after pair from ONE server
diffs at 0 px while proving nothing (FR-116 P3c, recorded in its PR).

⚠️ `ChatView` and `ConferenceView` own their own custom-flex layout and
**intentionally do not use `<v-container>`** — do not "fix" them to match.

## Build & test

```bash
cd ui
bun run dev            # Vite dev server :5000
bun run build          # vue-tsc --noEmit + production build
bun run test:unit      # Vitest (jsdom)
bun run e2e            # Playwright (32 spec files)
```

Test layout: unit specs in `ui/src/__tests__/` (stores, composables — including
the `rc:*` ws channel and `useRemoteControl` HID/button-mapping locks), E2E specs
in `ui/e2e/` with fixtures in `ui/e2e/fixtures/test-helpers.ts` — see
[testing.md](testing.md).
