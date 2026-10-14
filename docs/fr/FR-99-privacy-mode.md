# FR-99: Privacy mode — the person at the host sees a notice instead of the desktop, and the session ends locked

**Issue:** [#2091](https://github.com/gjovanov/roomler-ai/issues/2091) · **Status:** proposed 2026-10-13 — spec
and claim · **Owner:** the Windows agent (the user-context worker, SystemContext), the hub (the permission
bit), the viewer (the toggle) · **Anchors:** master `c2e79221b` · **Program:**
[StarDesk teardown](../plans/stardesk-teardown-program.md), wave 1 · **Builds on:**
[FR-27](FR-27-consent-surfaces-and-desktop-companion.md) (consent surfaces, P10 host Disconnect),
[FR-34](FR-34-locked-consent-and-stuck-capture.md) (the locked host), [FR-92](FR-92-keep-busy.md) (the
no-hooks rule)

## 1. Goal

An admin working on an unattended office PC should not show the room what they are doing. While a
privacy-mode session runs, the person at the host sees a notice ("this PC is being used remotely by …")
instead of the desktop, and cannot drive the session from the local keyboard and mouse. The controller sees
the real desktop. When the session ends, the PC is left locked if that is how it was found.

StarDesk ships this as "Privacy Mode" (black out the local displays, mute the host), "Turn Off Display" and
"Auto-lock after session". TeamViewer calls it "Show black screen". Roomler has none of it (§2).

**Nothing survives the session**, and the person at the screen can always take the machine back.

## 2. Evidence (master `c2e79221b`)

| What | Where | Consequence |
|---|---|---|
| No privacy mode, curtain, blank screen or local-input block anywhere | greps for `privacy`, `curtain`, `BlockInput`, `black_screen` hit nothing relevant | A full gap |
| `WDA_EXCLUDEFROMCAPTURE` already keeps **our own** windows out of the stream | `agents/roomlerd/src/indicator/win.rs` L515, L519 (border and badge); `agents/roomler-desktop/src/panels.rs` L408–414 (consent panels) | The capture-exclusion half of a curtain is a call we already make |
| A locked host streams a padlock frame, because capture cannot see `winsta0\Winlogon` from the Default desktop | `agents/roomlerd/src/lock_overlay.rs` L1–20, `lock_state.rs` | An ordinary window cannot appear on the secure desktop (FR-34 §1) |
| Low-level hooks, raw-input sinks, key polling and key taps are **banned** | `docs/fr/FR-92-keep-busy.md` L120–126: "EDR flags each as a keylogger, and GPO-locked corporate desktops with EDR are in the acceptance bar"; AC8 is a source scan that fails on a planted `SetWindowsHookEx` | The obvious input block (`WH_KEYBOARD_LL`) is off the table |
| The `rc:*` wire carries `Permissions` as **names**; an unknown name fails the whole frame | `crates/remote_control/src/permissions.rs` L15–22, and the deserialiser notes below it | A new permission bit must never reach an agent that does not know it |
| Every host-side Disconnect goes through one arm, which reports `host_disconnect` first | `agents/roomlerd/src/signaling.rs` L1935 (`kill_rx`), L139 | The person's way out already exists; the curtain must not take it away |
| Device-owned settings are kept out of server pushes structurally | `crates/remote_control/src/models.rs` L1718 (`remote_config_enabled` "deliberately ABSENT") | The opt-in for this mode follows the same rule |
| The consent floor is the device's | `agents/roomlerd/src/consent.rs` L153 (`strictest_of`) | The lock-on-end policy follows the same shape |
| No lock on disconnect, no host mute | — | Gaps |

## 3. Design

### 3a. The curtain (Windows 10 2004 and later)

The curtain lives in the **user-context worker**, the process already on `winsta0\Default` in the console
session:

- One window per monitor: topmost, layered, **hit-test-transparent** (`WS_EX_TRANSPARENT`, so the
  controller's injected clicks reach the application underneath), no-activate.
- It carries `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`, the same call `indicator/win.rs` L515
  makes. The local person sees the curtain; DXGI duplication and WGC see through it.
- It paints a localized notice: who is connected, that privacy mode is on, and how to take the machine back
  (§3b). That last line reads **"Press Ctrl+Alt+Del to take this computer back"** (decision 4).
- Z-order is re-asserted on `WM_WINDOWPOSCHANGING` and on a slow timer, so a topmost window opened by an
  application cannot uncover the desktop.

⚠️ **The secure desktop cannot carry a curtain** (FR-34 §1). When the input desktop is `Winlogon` (lock
screen, UAC, Ctrl+Alt+Del), the person sees that screen, which shows no user content, and SystemContext
keeps streaming it. Documented, not fought.

**As built, P1b (2026-10-14).** The curtain lives in the overlay that already draws the "being viewed" border,
badge and consent panel (`agents/roomlerd/src/indicator/win.rs`), on its pump thread:

- **One window over the whole virtual desktop**, not one per monitor, with the notice painted centred on each
  monitor. One window keeps a single z-order to defend and a single affinity to verify. Monitors are
  enumerated without the paint DC, whose clip would shrink them to the update region and move the text on a
  partial repaint. It refits on `WM_DISPLAYCHANGE` and on the slow timer.
- **The capture exclusion is set before the window is shown and read back after.** A curtain the capture
  could see would hide the desktop from the controller too, so an affinity that does not read back
  `WDA_EXCLUDEFROMCAPTURE` destroys the window and the request is refused (`no_curtain`).
- **Z-order:** every fourth pump tick (≈ 0.5 s) the curtain re-takes the top, then the border, the badge and
  the consent panel go back above it. The badge's Disconnect is the person's way out while only the curtain
  is up, and a consent question must never sit behind it. This replaces the `WM_WINDOWPOSCHANGING` idea:
  another application raising itself sends the curtain no message at all.
- `set_privacy` waits up to 2 s for the pump to say the curtain is up, and treats no answer as no curtain. A
  session's end (`hide_session`) always takes its curtain down, whatever its channel managed to say first.
- **The notice** says who is connected, that privacy mode is on, and, while there is no input block, how to
  end the session: "move the pointer to the top edge of the screen and choose Disconnect". P2 changes the
  last line to decision 4's "Press Ctrl+Alt+Del to take this computer back". English only for now.

⚠️ **The curtain hides the screen; it does not stop a person at the keyboard.** It must be click-through, or
the controller's injected clicks would land on it, so the person's own mouse passes through too. The shell
draws the Start menu, notifications and Alt+Tab's thumbnails above every topmost window, so those show over
it. Blocking local input is P2.

### 3b. Blocking local input, without hooks

FR-92's ban rules out `SetWindowsHookEx`. The block uses **`BlockInput(TRUE)`** instead (Microsoft Learn,
`BlockInput`):

- Physical keyboard and mouse input stops reaching applications. **Only the thread that called
  `BlockInput` can still inject** with `SendInput`, so the controller's input must be injected on that
  thread.
- The OS lifts the block **when that thread exits** and **when the person presses Ctrl+Alt+Del**. That is a
  dead-man switch the OS provides, and it is the person's way out.
- A Ctrl+Alt+Del is the person taking the machine back. The agent audits it, tells the controller, turns
  the curtain off and **never silently re-arms** the block.

**P0 has to prove four things before any of this ships:**

1. the integrity level `BlockInput` needs: the attended worker runs at the user's level, so this may need
   the SYSTEM-in-session context;
2. that injection from the blocking thread lands, for every input kind the session sends;
3. that FR-27 P10's Disconnect stays reachable for the person;
4. that EDR stays quiet, using FR-92's EDR-window method.

If any of them fails, this FR ships the **curtain alone** and says so in the consent prompt and the docs.
FR-92's source scan is extended to this module either way, so a hook can never be added later by accident.

**P0's answers (vmtest Win11, agent 0.4.134, 2026-10-13 and 14; field log rows 1–6).** Standalone probes
ran as the signed-in person through an `/IT` scheduled task. The person was QMP input, and the controller was
the production viewer from a LAN neighbour.

| # | Question | Answer |
|---|---|---|
| 1 | Integrity level | **High.** A medium-integrity caller gets `BlockInput(TRUE) = FALSE`, `ERROR_ACCESS_DENIED` (5). The signed-in user's elevated token succeeds. So the attended worker, which runs at the user's medium level, **cannot** block. The block needs the SystemContext worker (SYSTEM in session, untested but above high) or an elevated helper |
| 2 | Injection from the blocking thread | **Only that thread's `SendInput` lands.** The blocking thread's own move did land. The agent's injection, from the SystemContext worker (another process), was blocked like the person's: 73 L4 probe moves were lost in the batch spanning a 20 s block, against 0 in the batch before and 0, then 2, in the two after as the session recovered. The injector must therefore be the blocking thread itself. **Keys and the wheel follow the same rule** (row 6): the blocking thread's own keys and wheel notch arrived during the block, the person's keys did not, and keys typed after the unblock did. Mouse moves, virtual-key keystrokes and the wheel were tested; mouse buttons and `KEYEVENTF_UNICODE` text under the block were not, and P2's field cells carry them |
| 3 | The way out, and P10 Disconnect | **Ctrl+Alt+Del lifts the block**, and nobody tells the blocking thread. After the secure-attention sequence and Esc, the person's moves landed again while the probe still thought it was blocking. Its `BlockInput(FALSE)` then returned FALSE. So the agent has to detect the lift itself, audit `local_override` and never re-arm silently. **P10's Disconnect is not reachable under the block**, because it needs a click. It is reachable after Ctrl+Alt+Del. Settled by decision 4 (2026-10-14): Ctrl+Alt+Del is the way out |
| 4 | EDR | **Not tested.** vmtest guests run Defender only; FR-92's EDR-window method is still owed |

**The curtain, on the deployed capture path.** A topmost, click-through, no-activate window over the
primary monitor was given `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` (the call succeeds,
affinity `0x11`). The host's screenshot (what the person sees) showed the curtain, and the viewer's stream
showed the desktop under it. **The red half:** the same window without the affinity showed in the stream.
It holds on **both** capture paths: SystemContext's DXGI duplication (row 1) and the attended worker's WGC
(row 5).

### 3c. Authorization and audit

- **A new `Permissions::PRIVACY` bit.** The hub adds it **only** for agents that advertise the
  `privacy-mode` verb in `AgentCaps.rpc` (equality match; unknown verbs ignored). Any other agent would fail
  the whole frame on the unknown name.
- **The device opts in.** `privacy_mode_allowed` is device-owned, default off, and absent from
  `DesiredConfig`, like `remote_config_enabled`.
- **The consent prompt says it.** A prompt that grants privacy mode says the local screen will be covered.
- **Audit:** `privacy_on` and `privacy_off {reason}`. The reasons are `controller`, `session_end`,
  `local_override` (Ctrl+Alt+Del), `worker_exit` and `secure_desktop`.
- **No new `EndReason`.** An older server drops a frame carrying an unknown one (the FR-27 P10 lesson).
  Privacy mode ending does not end the session.

**As built, P1a (the server's gate, 2026-10-14).** Nothing reaches a device yet: no agent advertises the
words, and no viewer asks.

| Piece | Where | What it does |
|---|---|---|
| `Permissions::PRIVACY` | `crates/remote_control/src/permissions.rs` | Bit `0b0100_0000`. It is in `requires_consent_prompt`, never in the default grant |
| `privacy-mode`, `privacy-available` | `RpcCap::PrivacyMode`, `RpcCap::PrivacyAvailable` (`crates/remote_control/src/models.rs`) | `privacy-mode`: the owner allowed it and the agent serves it. `privacy-available`: a curtain is possible, but the owner has not allowed it, so the viewer can explain rather than offer nothing (FR-85's `available` lesson). Equality-matched; neither is a prefix of the other |
| `PrivacySupport` | `crates/modules/fleet/src/hub.rs` | `None` / `NotOptedIn` / `Serves`, read from those words per CONNECTION at the hello and on every caps re-announcement (`socket.rs`), so an owner's OFF stops the next grant without a reconnect |
| `privacy_grant` | `hub.rs`, in `create_session_full` right after `record_grant` | Strips `PRIVACY` before `rc:request` is built. Reasons, first match wins: `device_cannot_curtain`, `break_glass`, `device_not_opted_in` |
| The reason, twice | `SessionCreated.privacy_refused` (`signaling.rs`), `SessionRequested.privacy_stripped` (the audit) | A strip is never silent. Both are additive fields, so an older viewer and an older pod read past them |

- **Break-glass is the authz gate's `override_reason`.** The gate sets it only for a validated ADMINISTRATOR
  force. The hub ignores the wire's own field, and the cross-pod relay carries the gate's, so an owner pod
  applies decision 1 as well.
- **Stored grants tolerate a name they do not know.** `deserialize_stored` (and the raw-bson path) drops an
  unknown name instead of refusing the row, the fail-safe direction it already took for unknown bits. A
  session row holding `PRIVACY` must still read after a rollback. That protects only a rollback to a build
  that has the rule, which is why it ships here, before anything can write the name. The `rc:*` wire still
  refuses an unknown name.
- **The order of deployment is load-bearing.** P1a must be on every pod before an agent advertises the
  words (P1b) and before a viewer asks for `PRIVACY` (P1c): an older server cannot parse a request carrying
  the name, and an older agent fails the whole frame. The delegate crossing (`DelegateFrame::SessionParams`,
  service → worker) is the same wire inside one install, so P1b keeps both ends on one version or strips the
  bit at the crossing. *(P1b: moot. That delegation is macOS-only (FR-43), and a Mac never advertises the
  privacy words, so the hub never sends it `PRIVACY`.)*

Tests: `privacy_is_granted_only_off_break_glass_on_a_serving_device`,
`privacy_support_reads_the_rpc_words_by_equality` and `the_hub_strips_privacy_before_the_agent_can_see_it`
(`hub.rs`); `privacy_available_does_not_imply_privacy_mode` (`models.rs`); the storage-rule tests in
`permissions.rs`; and `crates/tests/src/privacy_gate_tests.rs`, four cells through real servers, each reading
the raw `rc:request` the agent received.

**As built, P1b (the device's half, 2026-10-14).**

| Piece | Where | What it does |
|---|---|---|
| `privacy_mode_allowed` | `crates/agent-core/src/config.rs`, `config_surface.rs` | The owner's switch, default off, live. Absent from `DesiredConfig` (named in `the_device_owned_refusals_are_not_pushable`, which also refuses any `privacy_*` key). Over the LocalAPI only the console person may set it (`crates/localapi/src/lib.rs`), as for keep busy: it is their screen |
| The words | `agents/roomlerd/src/privacy.rs` (`current_words`), appended in `signaling::stub_caps` | `privacy-available` where a curtain is possible here (Windows 10 2004+, i.e. build ≥ 19041, in a build whose overlay came up); `privacy-mode` only while the owner allows it too. Never in the memoized `detect()`, for the record words' reason; the heartbeat re-announces a change, so an OFF stops the next grant at the hub |
| The consent line | `signaling.rs`, the remote-control prompt's `detail` | "They may also cover this screen while they work." on every surface, and only when the grant holds `PRIVACY` and this device could act on it |
| The `privacy` channel | `privacy.rs` (`attach`, `attach_refusing`), routed in `peer.rs` beside `record` | Controller → `rc:privacy.set {id, on}` / `rc:privacy.status {id}`; device → `rc:privacy.state {id, on, reason?}` |
| `curtain-smoke` | `roomlerd curtain-smoke [--seconds N]` | Puts a curtain up through the production overlay and prints `possible=… up=…`: the field check that needs no server and no viewer |

The device decides again at every "on", in its own words:

| # | Gate | Refusal |
|---|---|---|
| 1 | The session's grant holds `PRIVACY` (when the channel opens) | `not_granted` |
| 2 | The owner's `privacy_mode_allowed`, read now (survives a compromised server) | `disabled_on_device` |
| 3 | A curtain is possible here | `unavailable` |
| 4 | The overlay put it up, verified out of capture | `no_curtain` |

Turning it off is never refused. The curtain also comes down when the session ends (the channel closes, or the
overlay drops the session) and when the owner switches the gate off, which the viewer is told unasked
(`disabled_on_device`).

Tests: `privacy::tests` (the gates, on/off, the owner's OFF, the end, the words, the consent line, the wire),
also run on the Windows build where a curtain can be possible; `tests/privacy_dc.rs`, the production handlers
over a loopback PeerConnection pair with a recorder for the overlay (named in CI's recorder step); and CI's
Windows clippy job now compiles `viewer-indicator`, which no PR lane had built. **Negative control:** with
gate 2 disabled, `the_owner_gate_refuses_before_the_overlay_is_asked` fails.

**As built, P1c (the viewer, 2026-10-14).** `ui/src/composables/usePrivacyMode.ts`, wired in
`useRemoteControl.ts` and drawn in `RemoteControl.vue`.

- **`PRIVACY` is asked for only when the device serves it** (`privacy-mode` in its caps, by equality;
  `privacy-available` earns nothing). Unlike RECORD, which every viewer asks for, a granted `PRIVACY` puts
  "may cover this screen" on the person's consent prompt, and a server before P1a cannot parse the name.
  The page's agent is read only when it is the one being asked, because a device switch can race the
  request.
- **The channel** opens from `rc:session.created`, only on a grant holding `PRIVACY`, for the reason
  `openRecordChannel` does. The toggle shows ON only when the device says so, keeps a refusal with its
  reason, shows the curtain coming down unasked (the owner's OFF, `disabled_on_device`), and reads a closed
  channel as the session ending the curtain.
- **The toolbar:** a toggle on a grant holding `PRIVACY`; a disabled control whose title gives the server's
  strip reason (`device_not_opted_in`, `break_glass`, or a code from a newer server); nothing at all where
  no curtain could ever be (`device_cannot_curtain`). A refusal or an unasked take-down is also said in a
  snackbar, because the icon alone changes too quietly.

Tests: `ui/src/__tests__/composables/usePrivacyMode.spec.ts` (11, through a fake channel); the whole vitest
suite and `vue-tsc` green.

### 3d. Other platforms

- **macOS:** out. Since macOS 15 there is **no public API** that keeps a window out of screen capture. Apple
  DTS says so, and `NSWindow.sharingType = .none` is ignored by ScreenCaptureKit.
- **Linux X11:** a later phase may try DPMS-off. Injected input wakes the display, so it has to be
  re-asserted.
- **Wayland:** out.
- **The all-monitors-off variant** (stream a virtual display while the physical ones are dark) waits for
  the virtual-display FR (wave 2). It needs a topology dead-man and a boot reconciler, because unlike the
  curtain it changes persistent state.

### 3e. Lock when the session ends

`lock_on_end = auto | always | never`:

- **`auto`** restores what was found: lock if the session started on a locked screen or on an unattended
  desktop.
- The device's setting is a **floor**, as in `consent::strictest_of`. The server may ask for stricter,
  never looser.
- **Mechanisms:** Windows calls `LockWorkStation()` from the worker; Linux runs
  `loginctl lock-session <id>`. P0 picks the macOS mechanism.
- It runs on every end, including a host Disconnect and a controller crash.

### 3f. Host mute

**P0 measures** whether WASAPI loopback still captures with the render endpoint muted. StarDesk's own help
says muting the host silences the client too. The FR ships only the honest version:
- if loopback survives, "mute host" mutes the speakers and keeps remote audio;
- if it doesn't, the toggle says remote audio goes silent too.

## 4. Phases

| P | Scope | Kill switch | Status |
|---|---|---|---|
| P0 | This spec and the claim. The feasibility probes: `BlockInput` (the four questions in §3b), loopback under mute, the macOS lock mechanism | docs only | spec merged. **`BlockInput` and the curtain probed on vmtest 2026-10-13 and 14** (§3b's answers): high integrity needed, only the blocking thread injects (mouse, keys and the wheel alike), Ctrl+Alt+Del lifts the block, and the curtain stays out of both DXGI and WGC capture. Still owed: EDR, loopback under mute, the macOS lock |
| P1 | The curtain + the permission bit + the device opt-in + audit (Windows), in three steps below | `privacy_mode_allowed=false` (default) | P1a and P1b merged; P1c in review |
| P1a | The server's gate: the bit, the two `rpc` words, the hub's strip with its reasons, break-glass excluded, the storage rule (§3c as built) | none needed: nothing advertises the words yet | **merged** #2186 (`2a1e939de`): unit + `privacy_gate_tests`; the negative control (strip disabled) fails them |
| P1b | The agent: `privacy_mode_allowed`, the words, the consent line, the curtain, the `privacy` channel, and the device's own refusal (§3a and §3c as built) | `privacy_mode_allowed=false` | **merged** #2191 (`c4df7ecde`): Linux unit + `privacy_dc` 6/6, Windows clippy and units with `viewer-indicator`; NC fails as it should. **Field-verified** with `curtain-smoke` under both capture paths (§8 rows 7 and 8) |
| P1c | The viewer: the toggle, the refusal reasons, asking for `PRIVACY` only from a device that serves it (§3c as built) | the toggle | **#2199**: `usePrivacyMode.spec.ts` (11), the vitest suite and `vue-tsc`. The end-to-end field cell (AC1: the toggle reaching a curtain through the channel) needs P1c deployed and an agent release carrying P1b |
| P2 | The input block, if P0 cleared it | `privacy_input_block=false` | — |
| P3 | Lock on end (Windows, Linux, macOS) | `lock_on_end=never` | — |
| P4 | Host mute | the toggle | — |
| P5 | Docs and the field table | — | — |

## 5. Acceptance criteria

- [ ] **AC1 (the curtain, red first):** on a vmtest Win11 guest during a session, `virsh screenshot` (what a
  person at the screen sees) shows the desktop on the deployed build. With privacy on it shows the curtain,
  while the Playwright viewer from mars sees lit desktop pixels in the same second.
- [ ] **AC2 (local input):** the "person" (QMP `input-send-event`, `virsh send-key`) does not reach
  `roomlerd probe-target` while privacy is on, and remote input does. The negative control comes first:
  without privacy, the person's input does reach it.
- [ ] **AC3 (the way out, reworded 2026-10-14 by decision 4):** Ctrl+Alt+Del lifts the block. The agent
  detects the lift itself (nobody tells the blocking thread, P0 row 4), audits `local_override`, tells the
  controller, and takes the curtain down. FR-27 P10's Disconnect is then one click away. With the curtain
  alone (no block), Disconnect works while privacy is on. *As first written, AC3 said Disconnect works
  "while privacy is on", which P0 showed cannot hold under the block: `BlockInput` swallows the click.*
- [ ] **AC4 (nothing survives):** `taskkill /F` on the worker clears the curtain and the block within 2 s,
  and `privacy_off {worker_exit}` is audited. After a session end, nothing of privacy mode remains.
- [ ] **AC5 (the gate holds):** an agent without the `privacy-mode` verb never receives `PRIVACY` (an
  integration test against the hub). A device that did not opt in refuses it, with a visible reason.
- [ ] **AC6 (no hooks):** the FR-92-style source scan covers this module and fails on a planted
  `SetWindowsHookEx`.
- [ ] **AC7 (lock on end):** with `auto`, a session that began on a locked guest leaves it locked
  (`Get-Process LogonUI` present). One that began unlocked and attended leaves it unlocked. `always` locks
  both.
- [ ] **AC8:** **Docs:** a `docs/privacy-mode.md` page with a mermaid diagram of the curtain, the block and
  the way out, the per-OS table, and the honest limits (secure desktop, macOS), linked from
  `docs/README.md`. `docs/remote-control.md`'s consent section links it.

## 6. Decisions

**All four were decided by the operator on 2026-10-14, as recommended.** Each entry keeps what was
weighed.

1. **ADMINISTRATOR break-glass.** May a break-glass session turn privacy on? Recording is never allowed
   under break-glass. Recommended: the same rule. A break-glass session is audited and the owner notified,
   and covering the screen would hide it from the one person who could object. **Decided: no.** The hub
   never grants `PRIVACY` under break-glass, as it never grants `RECORD`.
2. **A whitelisted chord** besides Ctrl+Alt+Del, for the person to end privacy without the secure desktop.
   Recommended: no. `BlockInput` swallows it anyway, and Ctrl+Alt+Del is the one path that cannot be faked
   by software. **Decided: no.**
3. **Default `lock_on_end`.** Recommended: `auto` on new installs, `never` on existing ones until the field
   cells pass. **Decided: as recommended.**
4. **How the person reaches P10's Disconnect under the block** (raised by P0, 2026-10-13). `BlockInput`
   swallows the click, so Disconnect is reachable only after Ctrl+Alt+Del, which ends the block anyway.
   AC3 says "Disconnect works while privacy is on", which holds for the curtain alone but not under the
   block. The options:
   - Ctrl+Alt+Del is the person's path. It lifts the block, the agent sees the lift and audits
     `local_override`, and Disconnect is then one click away. AC3 is reworded to say so;
   - privacy without the block (the curtain alone) keeps Disconnect clickable, and the block becomes a
     separate, stricter level.

   Recommended: the first, and the curtain's notice says "Press Ctrl+Alt+Del to take this computer back".
   **Decided: the first.** AC3 is reworded, and §3a carries the notice's wording.

## 7. Out of scope

- macOS and Wayland curtains (no supported mechanism).
- The all-monitors-off variant (it needs the virtual display).
- Hiding the session from the device's owner: the on-host indicator rules are unchanged.

## 8. Field-verification log

| # | Date | Cell | Result |
|---|---|---|---|
| 1 | 2026-10-13 | **P0 probe, not the product.** vmtest Win11 guest, agent 0.4.134 (SystemContext), a standalone WinForms curtain run as the person; the production viewer from a LAN neighbour | **The curtain stays out of DXGI capture.** Without the affinity (the red half), the stream showed the magenta curtain. With `WDA_EXCLUDEFROMCAPTURE` (`SetWindowDisplayAffinity = True`, affinity `0x11`), the host's screenshot showed the curtain while the viewer's stream showed the companion's Welcome window under it |
| 2 | 2026-10-13 | The same guest, probe-target and the viewer in L4 target mode, `BlockInput` probes as the person | **The integrity level, and who may inject.** Elevated: `BlockInput(TRUE) = True`; the cursor froze for 20 s; the agent's injected probe moves were lost (73 in the batch spanning the block; 0 in the batch before, 0 and then 2 in the two after); the probe's own `SendInput` landed (cursor to 200,200). Not elevated: `BlockInput(TRUE) = False, err 5`, and the moves kept landing |
| 3 | 2026-10-13 | The same guest, no viewer, so the person (QMP `mouse_move`) is the only thing moving the cursor | **The person is blocked, with a control.** With no block, the person's move moved the cursor (to 1279,799). Under the block, two moves changed nothing (it stayed at 200,200) |
| 4 | 2026-10-13 | The same, then `virsh send-key` Ctrl+Alt+Del and Esc during the block | **The way out.** At Ctrl+Alt+Del the input desktop switched (`GetCursorPos` read 0,0). After Esc the person's next move landed (1279,799) while the probe still believed it was blocking. Its `BlockInput(FALSE)` then returned FALSE. The OS lifted the block and told no one |
| 5 | 2026-10-14 | A fresh guest, **attended**: the person signed in, so the agent captured through WGC (its log reads `wgc: capture session closed` at the viewer's disconnect). Agent 0.4.134, the same curtain probe, the production viewer from a LAN neighbour | **The curtain stays out of WGC capture too.** Without the affinity, the host's screenshot and the stream both showed the curtain. With it (`True`, affinity `0x11`), the host's screenshot showed the curtain, and the stream, sampled 11 s into the curtain's 30 s, showed the companion's Welcome window under it |
| 6 | 2026-10-14 | The same guest, no viewer. A focused window, elevated, blocks and injects from its UI thread: the keys O W N and one wheel notch. The person types over `virsh send-key`: P Q during the block, R S after it | **Keys and the wheel behave like the mouse.** Control (no block): the window received `ownpqrs` and the notch (120). Under the block (`BlockInput(TRUE) = True`), it held `own` and the notch at the unblock, so the person's P Q, typed about 3 s into the 13 s block, were dropped. R S, typed about 3 s after the unblock, arrived: `ownrs` |
| 7 | 2026-10-14 | **P1b's production overlay, SystemContext (DXGI) capture.** A vmtest Win11 guest whose installed agent (0.4.134) streamed to the production viewer from a LAN neighbour. `roomlerd curtain-smoke`, built from #2191 (sha256 `c35811fb…`), ran as the signed-in person | **The curtain is up for the person and out of the capture.** Before: the host's screenshot and the stream both showed the desktop. During, 11 s in: the host's screenshot showed the curtain and its notice, with the red border kept above it; the stream showed the desktop under it (the companion, and the console that ran the smoke). After: the desktop again. The guest's log: `privacy: curtain up, out of capture` at 08:05:42Z (the affinity read back), `curtain down` 40 s later; `curtain-smoke: possible=true up=true` |
| 8 | 2026-10-14 | The same check on an **attended** guest, so the agent captured through WGC | **The same, under WGC.** The host's screenshot showed the curtain, and the stream (VP9 4:4:4 on a direct path) showed the desktop under it: the companion, the console, the taskbar. `curtain up, out of capture` at 10:55:36Z, down 40 s later |

Attended runs happen on the operator's spare box, never on a machine someone is using.

## 9. Related

- [FR-27](FR-27-consent-surfaces-and-desktop-companion.md): consent surfaces, P10 host Disconnect.
- [FR-34](FR-34-locked-consent-and-stuck-capture.md): why the secure desktop cannot carry a window.
- [FR-92](FR-92-keep-busy.md): the no-hooks rule and its source scan.
- The program: [`docs/plans/stardesk-teardown-program.md`](../plans/stardesk-teardown-program.md).
