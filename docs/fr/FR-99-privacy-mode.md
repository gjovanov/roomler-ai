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
  (§3b).
- Z-order is re-asserted on `WM_WINDOWPOSCHANGING` and on a slow timer, so a topmost window opened by an
  application cannot uncover the desktop.

⚠️ **The secure desktop cannot carry a curtain** (FR-34 §1). When the input desktop is `Winlogon` (lock
screen, UAC, Ctrl+Alt+Del), the person sees that screen, which shows no user content, and SystemContext
keeps streaming it. Documented, not fought.

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
| P0 | This spec and the claim. The feasibility probes: `BlockInput` (the four questions in §3b), loopback under mute, the macOS lock mechanism | docs only | this PR |
| P1 | The curtain + the permission bit + the device opt-in + audit (Windows) | `privacy_mode_allowed=false` (default) | — |
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
- [ ] **AC3 (the way out):** Ctrl+Alt+Del lifts the block. The agent audits `local_override`, the
  controller is told, and the curtain goes. FR-27 P10's Disconnect works while privacy is on.
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

## 6. Open decisions

1. **ADMINISTRATOR break-glass.** May a break-glass session turn privacy on? Recording is never allowed
   under break-glass. Recommended: the same rule. A break-glass session is audited and the owner notified,
   and covering the screen would hide it from the one person who could object.
2. **A whitelisted chord** besides Ctrl+Alt+Del, for the person to end privacy without the secure desktop.
   Recommended: no. `BlockInput` swallows it anyway, and Ctrl+Alt+Del is the one path that cannot be faked
   by software.
3. **Default `lock_on_end`.** Recommended: `auto` on new installs, `never` on existing ones until the field
   cells pass.

## 7. Out of scope

- macOS and Wayland curtains (no supported mechanism).
- The all-monitors-off variant (it needs the virtual display).
- Hiding the session from the device's owner: the on-host indicator rules are unchanged.

## 8. Field-verification log

| # | Date | Cell | Result |
|---|---|---|---|
| — | — | — | — |

Attended runs happen on the operator's spare box, never on a machine someone is using.

## 9. Related

- [FR-27](FR-27-consent-surfaces-and-desktop-companion.md): consent surfaces, P10 host Disconnect.
- [FR-34](FR-34-locked-consent-and-stuck-capture.md): why the secure desktop cannot carry a window.
- [FR-92](FR-92-keep-busy.md): the no-hooks rule and its source scan.
- The program: [`docs/plans/stardesk-teardown-program.md`](../plans/stardesk-teardown-program.md).
