# FR-92: Keep busy — the agent keeps a host active with mouse patterns, and any real user takes over at once

**Issue:** [#1942](https://github.com/gjovanov/roomler-ai/issues/1942) · **Status:** in progress — P0–P9 merged; 0.4.125 released and field-verified 2026-10-11 (§9)
· **Owner:** remote desktop (agent + viewer), fleet (the org switch) · **Anchors:**
master `83aede0bd` · **Builds on:** [FR-55](FR-55-always-reachable.md) (power),
[FR-27](FR-27-consent-surfaces-and-desktop-companion.md) (consent, the companion banner, the person socket),
[FR-85](FR-85-hq-screen-recording.md) (`record_remote_enabled`, the device-owned key precedent),
[FR-43](FR-43-macos-single-enrollment.md) (the supervised macOS worker), the P6 input arbiter

## 1. Goal

An operator switches **keep busy** on from the remote-desktop viewer. The agent then moves the host's
mouse in a pattern so the host stays active: no screensaver, no idle lock, no "Away" in Teams or Slack.

The patterns:
- the geometric ones: circle, triangle, square, a one-stroke star, figure-8, heart, spirograph, an 8-petal rose, a wave;
- a Lissajous knot that morphs each loop;
- **wander**, a smooth random path with uneven speed and short rests, which looks the most human;
- **subtle**, a 1 px nudge every ~45 s that resets the idle clock with no visible motion;
- **shuffle**, which rotates through the visible ones.

**A real user always wins.** Any real input pauses it at once: the person at the machine, or a remote
controller, moving, clicking, typing or scrolling. It resumes once that person has been idle for
`resume_after` (30 s by default), and keeps going until someone switches it off. It survives
disconnects, self-updates and reboots.

**Operator decisions (2026-10-10):**
1. **Lifetime:** on until switched off, even across reboots. The state is kept **per signed-in
   person**: it resumes at that person's next login, and someone else signing in starts with it off.
   An optional auto-off is offered.
2. **Availability:** on by default for any session holding `INPUT`. The device owner can turn it off
   (`keep_busy_enabled`, never server-settable), and an **org admin can deny it org-wide** in v1. The
   deny also stops it where it is already running.
3. **Host side:** a companion tray item ("Keep busy is on · Stop") and `roomler keep-busy status|off`.

## 2. Evidence (master `83aede0bd`, 2026-10-10)

| What | Where | Consequence |
|---|---|---|
| FR-55 holds `PowerCreateRequest(PowerRequestSystemRequired)` / `IOPMAssertion("PreventUserIdleSystemSleep")` | `agents/roomlerd/src/power.rs` L305-471 | stops sleep, but not the screensaver, the idle lock or presence, because none of these reset the user-idle clock |
| Injected input does reset it: macOS logs a `UserIsActive` assertion "tickle … process:roomlerd" during a session | FR-55 field log; `power.rs` L17-22 | the mechanism works, but today it only runs while a controller types |
| One process-global injection worker owns the only OS injector, created lazily and **cached for the life of the process, Noop included** | `input/arbiter.rs` L581 `global()`, L646 `worker`, L652, L781 `get_or_insert_with` | a second injector would break modifier fencing and the portal route; an early Noop would break RC input until restart |
| Every RC input event already passes through `ArbiterState::plan` with an exact per-session held-key/button set | `arbiter.rs` L286-355, L125-131 | the engine gets an exact "a controller is active" signal, and knows when a remote button is down |
| Clicks carry coordinates, and dispatch **moves before every button event** | `input/mod.rs` L167-173; `input/enigo_backend.rs` L123-138 | a controller's click lands where they point, even after the engine moved the cursor |
| enigo 0.6.1's Windows `move_mouse(Abs)` is `SendInput(ABSOLUTE)` without `VIRTUALDESK`, normalised to the primary display; `to_pixels` ignores `mon` by default | `enigo-0.6.1/src/win/win_impl.rs` L175-176; `enigo_backend.rs` L293-308 | moving through `InputMsg` cannot reach a secondary monitor, so keep-busy needs its own pixel-space move |
| No idle-clock reader exists anywhere (`GetLastInputInfo`, `CGEventSourceSecondsSinceLastEventType`, `XScreenSaverQueryInfo`, Mutter IdleMonitor: 0 hits) | `agents/`, `crates/` | detection is new code; `Win32_UI_Input_KeyboardAndMouse` is already enabled (`agents/roomlerd/Cargo.toml` L425) |
| Lock state is Windows-only and per session; macOS/Linux stubs always read `Unlocked`, and the RC input gate drops input while `Locked` | `lock_state.rs` L170-214, L290-306, L313-335; `peer.rs` L8088-8112 | keep-busy needs its own lock probes on macOS/Linux, and they must **not** feed the RC gate (a Mac controller types the password on the lock screen) |
| The Windows worker lives for the whole console session, regardless of RC, and is respawned at login | `win_service/supervisor.rs` `decide_spawn` L664-763 | an engine there can outlive any session |
| The machine-global config dir is hardened to SYSTEM + Administrators; the default worker runs as the user | `crates/agent-core/src/config.rs` `harden_machine_global_dir`; `supervisor.rs` L816-880 | state "beside the config" is not writable by every worker, so it goes in the signed-in person's profile (`win_identity.rs` L104 `session_user_profile_dir`) |
| Control-DC verbs are untyped JSON switched on `t`; the arbiter fans state out to every session; the viewer drops unknown `t` | `peer.rs` L8239-8699; `arbiter.rs` L789-825; `ui/src/composables/useRemoteControl.ts` L966 | new verbs need no baseline re-record and no server change |
| Org kill switches live in `TenantSettings` (`remote_exec_enabled`, `remote_ssh_enabled`); reconcile-on-connect pushes are cap-gated; cross-pod hub ops go through `publish_rc_ctrl` | `crates/db/src/models/tenant.rs` L79-165; `crates/modules/fleet/src/socket.rs` L240-290; `fleet/src/ctrl.rs` L21-45 | the org deny has a precedent for each of its parts |
| The person-at-the-device LocalAPI socket serves an exhaustive allowlist | `crates/localapi/src/lib.rs` L2235 `person_may` | the Stop must be added there, so a Linux system install's companion can reach it |

## 3. Design

### 3a. One more input source, not a second injector

```mermaid
flowchart LR
  V[viewer] -- "rc:keep-busy.set (control DC)" --> P[peer.rs control handler]
  P -- INPUT + policy --> E[keep_busy engine thread]
  E -- "KbReq: Probe / Locate / MovePx" --> A[input arbiter worker]
  A -- the one injector --> OS[(OS cursor)]
  A -- "controller event injected" --> E
  E -- "watch: state" --> P2[per-session emitters] -- "rc:keep-busy.state" --> V
  E -- watch --> L[LocalAPI / heartbeat]
```

- **New module `agents/roomlerd/src/keep_busy/`**: one std thread. It contains:
  - `engine.rs`: a pure state machine over a `Host` trait, tested with a fake host and clock, the
    `ArbiterState` split again;
  - `patterns.rs`, `state.rs`;
  - per-OS host probes (idle clock, lock, mouse buttons, work area).
  
  File I/O and lock probes run on this thread, never on the input path.
- **The arbiter serves requests.** `Cmd::KeepBusy(KbReq{ op: Probe | Locate | MovePx{x,y}, reply })`:
  - Reads run on the injector thread, so the SystemContext desktop rebind covers them, with a reply
    timeout so a stuck injector can't hang the engine.
  - `MovePx` is **refused while any session holds a mouse button**.
  - After every *session-caused* injection (never a `Heartbeat`, never a denied event), the arbiter
    notes remote activity.
- **The injector trait gains `cursor_px` / `move_px`** in desktop pixel space. They default to
  unsupported (Noop, or a portal-owned route ⇒ `unavailable:portal`, never a silent no-op).
  - Windows: `move_px` is a direct `SendInput(MOVE|ABSOLUTE|VIRTUALDESK)` over the virtual screen, and
    `cursor_px` is `GetCursorPos` (the process is per-monitor-v2).
  - macOS/X11: enigo's `move_mouse(Abs)` and `location()` share global/root coordinates.
  - The read and the write use one coordinate space, so a divergence means something. The pixel moves
    bypass enigo `dispatch`, so they never spend the 50 INFO input-diagnostic samples or the keyboard
    layout sampler.
- **Start** after `arbiter::global()` exists (it captures the tokio handle). The engine homes:
  - **Windows:** the console-session worker.
  - **macOS:** the GUI-session worker, which runs the full `run()`.
  - **Linux:** the daemon.

### 3b. Who is a real user — an expected-state diff

The engine only ever causes absolute mouse moves, so anything else is a human. Each tick it pauses on:

1. **An idle-clock advance it did not cause.**
   - **Windows `GetLastInputInfo`.** It's session-scoped and updated asynchronously, in 15.6 ms ticks.
     After its own move the engine polls until `dwTime` advances (cap ~50 ms) and keeps that as the
     baseline. A later `dwTime` ≠ baseline (a wrapping u32 compare) means a human. No advance at all
     means the move is not landing (UIPI, the wrong desktop) ⇒ `not_landing`, never "busy".
   - **macOS `CGEventSourceSecondsSinceLastEventType`.**
   - **X11 `XScreenSaverQueryInfo`** (the x11rb `screensaver` feature).
   - **A host with no idle clock** does not advertise the feature: v1 refuses rather than run
     keyboard-blind.
2. **Cursor divergence:** the cursor is more than 1 px from where the engine left it.
3. **Controller input** noted by the arbiter.

Divergence with no clock evidence means an app warped or confined the cursor. That's
`cursor_contended`, with exponential backoff, so it never oscillates.

**Banned:**
- `SetWindowsHookEx` (low-level hooks);
- `RegisterRawInputDevices(INPUTSINK)`;
- keyboard `GetAsyncKeyState` polling;
- key event taps.

EDR flags each as a keylogger, and GPO-locked corporate desktops with EDR are in the acceptance bar.
Mouse-*button* state is read **once per resume**, never polled.

**Calibration** on every enable and resume: read the cursor, move it **3 px out and back**, verify the
pointer reached each leg, and require the idle clock to register the moves. Otherwise
`unavailable:calibration_failed` (or `not_landing` when the clock never moves).

Two details matter:
- A move to the same point may not count as input anywhere.
- A pointer stuck at the anchor round-trips "home → home" perfectly. Only checking the out-leg catches it.

A test pins this (P1).

**Resume** when the human has been idle for `resume_after`, no button is held, the session is unlocked
and present. The engine re-anchors at the **current** cursor and continues from the nearest point of the
pattern: no jump, an eased start.

```mermaid
stateDiagram-v2
  [*] --> Off
  Off --> Calibrating: set on (INPUT, allowed)
  Calibrating --> Running: round-trip ok
  Calibrating --> Unavailable: no round-trip / no clock
  Running --> Paused: human (clock, cursor, controller)
  Paused --> Calibrating: idle ≥ resume_after, no button
  Running --> Locked: lock / secure desktop / no session
  Paused --> Locked
  Locked --> Calibrating: unlocked
  Running --> Off: set off / tray Stop / org deny / owner off / auto-off
  Paused --> Off
  Locked --> Off
```

### 3c. Safety invariants

- **Moves only**: never a click, key or scroll.
- **Safe box**: the work area of the monitor under the anchor, intersected with a 48 px inset from
  every monitor edge. It stays out of hot corners and edges: GNOME Activities, macOS hot corners (which
  can start the screensaver or **lock the screen**), auto-hide taskbars and Docks, the menu bar, Peek.
- **Pause while locked, on a secure desktop, or with no session.**
  - Windows: `probe_lock_state_detailed()`, a free function called per tick. It is not the
    session-scoped monitor (#1738).
  - macOS: `CGSSessionScreenIsLocked`.
  - Linux: logind `LockedHint`.
  - These probes are **keep-busy-only**.
- **Never the first caller of the cached injector before a display exists.** No `Locate`/`MovePx`
  until `Probe` confirms an interactive session. A boot-time restore on a Linux daemon without
  `DISPLAY` would otherwise cache a Noop and break RC input until restart.
- **The viewer never re-asserts keep-busy on connect.** It's the opposite of display-match
  (`RemoteControl.vue` ~L4148): the device's state is authoritative, and a reconnecting tab must not
  undo the host's Stop.
- **Wayland is refused explicitly** (loginctl `Type=wayland`). XTest there moves only Xwayland's
  private pointer, so a calibration could pass while nothing real moves.
- **Warnings are reported, not swallowed:**
  - `focus_follows_mouse` (Windows `SPI_GETACTIVEWINDOWTRACKING`; the viewer suggests `subtle`);
  - `resume_after_exceeds_lock_timeout`.

### 3d. Wire

- **Control DC in:**
  - `{"t":"rc:keep-busy.set","on":true,"pattern":"circle","size":"m","speed":"normal","resume_after_s":30,"auto_off_min":null}`
  - `{"t":"rc:keep-busy.set","on":false}`
  - `{"t":"rc:keep-busy.get"}`
  
  Each requires `INPUT`.
- **Control DC out** (to every session on change, and on open/get): `{"t":"rc:keep-busy.state","available":true,"on":true,"phase":"calibrating|running|paused|locked|off|unavailable","reason":"…","paused_by":"local|remote|null","resumes_in_ms":23000,"pattern":"circle","size":"m","speed":"normal","resume_after_s":30,"auto_off_at_ms":null,"set_by":"Alice","detector":"clock+cursor","warn":[]}`.
  - `reason` is a closed enum with wire codes; the sentence is written on the agent (the
    `apps::Unavailable` pattern).
- **Caps:** `keep-busy` in `AgentCaps.input` (`encode/caps.rs` L1445). It's present only when the build,
  the host, the idle clock and `keep_busy_enabled` allow it, and it follows the key at heartbeat time,
  like the `record` cap. Matching is equality, and unknown words are ignored.
- **LocalAPI:** `keep_busy_status` / `keep_busy_off` → `keep_busy`.
- **Server → agent:** `ServerMsg::KeepBusyPolicy { denied }`, tag `rc:agent.keep_busy_policy`,
  owned by fleet in `namespace()`. It is the only server→agent keep-busy message (§3f).

### 3e. Persistence — per signed-in person

- **The file:** `keep-busy.json` (version, on, settings, `set_by`, `set_at`, an absolute `auto_off_at`,
  last-known org denies, `#[serde(flatten)] extra`).
  - Atomic temp+rename, copied from `crates/agent-core/src/desktop_state.rs`.
  - Written on On/Off edges only, never on pause/resume.
  - Corrupt ⇒ Off.
- **Location:**
  - **Windows:** the console user's profile (`win_identity::session_user_profile_dir` under SYSTEM, else
    `own_profile_dir`). The user-context and SystemContext workers resolve the same file.
  - **macOS:** the GUI worker's `data_local_dir`.
  - **Linux root daemon:** `keep-busy-<console uid>.json` next to the logs.
- **Never stored in `config.toml`.** It's SYSTEM/root-owned on system installs, sits behind an
  in-process write lock, gets reported to the server, and `adopt_local` would read every toggle as a
  config edit.

### 3f. Policy gates

- **Device: `keep_busy_enabled`**, default true (`AgentConfig`, near `power_policy` L167).
  - It's a live `config_surface` key in RemoteDesktop, like `record_remote_enabled` (L1590), and goes
    in `live_keys_are_exactly_the_adopt_local_set` (L2699).
  - A local `ConfigSet` takes the console-user gate of `record_*` (`localapi/src/lib.rs` ~L2431).
  - It is **absent from `DesiredConfig`**, pinned by a twin of
    `no_record_key_is_server_pushable_via_desired_config` (`models.rs` L4448).
  - Its description says that keep-busy overrides `power_policy = never`, because injected input
    defeats idle sleep.
- **Org: `TenantSettings.keep_busy_denied`** (default false = allowed).
  - `GET/PUT /api/tenant/{tid}/keep-busy-settings`: reading needs MANAGE_AGENTS, writing needs
    MANAGE_TENANT, following exec-settings (`fleet/src/agent_exec.rs`).
  - **Standing:** reconciled on connect after `ConfigPush` (both `true` and `false`, so a re-allow while
    offline clears), and pushed on change through `publish_rc_ctrl` → an `apply_rc_ctrl` arm, because
    the PUT can land on any pod. Both are cap-gated (`supports_keep_busy`).
  - **The word reappearing counts as a connect.** When the owner turns `keep_busy_enabled` back
    on, the agent re-announces its caps on a heartbeat, and the policy is pushed then. Without
    this, a deny made while the word was absent would never arrive. It is pushed only on the
    reappearance: caps are also re-announced for the record word and for a worker's permissions.
  - **On the agent:** the effective deny is the strictest of every enrolled org's last-known deny,
    persisted. An old server (no push) means allowed.
  - **An org the device has left drops out.** At each start, a stored policy from an org that is
    no longer the primary or in `[[orgs]]` is dropped (`prune_departed_orgs`). Otherwise leaving
    an org that had denied keep busy would deny it on that device for good, because no server
    would ever push that org's re-allow.
  - **Device-wide, so no per-session field.** Under strictest-of a deny is device-wide, and the
    engine itself refuses every enable while it holds (`org_denied`, to every viewer). The connect
    reconcile reaches the agent before any session request, and a change is pushed at once. A
    `keep_busy_denied` field on `ServerMsg::Request` would only repeat that, at the cost of threading
    it through the session authz, the hub, the cross-pod relay JSON and the macOS delegation. That
    design was dropped in P1 (see §8).

### 3g. Host side

- **LocalAPI `KeepBusyStatus` / `KeepBusyOff`**, both on the person-socket allowlist.
  - There is no local enable verb.
  - Off is open to every peer the endpoint admits (including `roomler exec` as SYSTEM/root), because
    stopping is the safe direction.
- **CLI:** `roomler keep-busy [status|off] [--json]`.
- **Companion:**
  - a tray item and tooltip, polled like the recording item (`roomler-desktop/src/tray.rs`
    `spawn_recording_watch`);
  - a one-shot notification when it turns on;
  - a "Keep busy is on · Stop" row in the viewing banner while that banner is up.

### 3h. Viewer and admin UI

- **Viewer composable:** `useKeepBusy.ts`, modelled on `useRemoteRecording.ts`.
- **Viewer menu:** sits beside Record and holds
  - an on/off switch;
  - a pattern grid of inline-SVG previews drawn from the same formulas;
  - speed and size;
  - `resume_after` (10 s – 5 min) and auto-off (never – 8 h);
  - a status line ("Paused, someone is using this computer · resumes in 23 s");
  - a live chip like Host-locked.
- **Gating:** on the cap word and the session's `INPUT` bit; an unavailable or denied state is shown
  with its reason.
- **Org admin:** a switch in `SettingsSection.vue` beside exec/SSH; since P8 a default-pattern
  select beside it, and since P7 the History list (when · device · what · who, a device-reported
  vs admin marker, a device filter, "load more").
- **Device list (P5b):** a "keep busy · <phase>" chip, shown while it is on, naming the pattern and who
  turned it on. It is driven by an additive `AgentHeartbeat.keep_busy` (a device *claim*, shown and never
  enforced on), persisted by `touch_heartbeat`, and **unset** when a device stops saying (an older
  build, a supervised Mac's root daemon, which cannot speak for its GUI worker). So it reads as
  unknown, never a stale "on" or an invented "off". A durable transition audit is an open decision (§6).

## 4. Phases

| Phase | What | Kill switch | Status |
|---|---|---|---|
| P0 | Claim: issue #1942, this spec, the ledger row | Docs only | #1943, merged `058d6304a` |
| P1 | Pure engine + patterns + fake-host tests; `cursor_px`/`move_px` + the arbiter `KbReq` seam; the Windows host; the control-DC verbs; the state file; `keep_busy_enabled`; the cap word | `keep_busy_enabled = false`; no cap word ⇒ the viewer hides it | #1948, merged `a9fc3f0de` |
| P2 | Viewer: composable, menu, previews, chip | the cap word | #1949, merged `28fd63c19` |
| P3 | **P3a, X11**: the X11 host (MIT-SCREEN-SAVER idle clock read as an interval, logind `LockedHint`, RandR ∩ `_NET_WORKAREA`), Xwayland refused from the display itself (`wayland`), the person followed under a root daemon or a SYSTEM worker, proven on Xvfb in CI. **P3b, macOS**: the CoreGraphics host, and the daemon handing every org's policy to the GUI worker | per-host cap word | P3a #1959, merged `f47dfb5a1`; P3b #1960, merged `3a9fb7b12` |
| P4 | LocalAPI verbs (person socket), CLI, companion tray item. Not built: the OS notification (the companion has no notification plugin yet) and the viewing-banner row (the banner shows only during a session; the tray is the persistent surface) | — | #1951, merged `51dfd2466` |
| P5 | Org deny: tenant key, routes, admin UI; a standing `rc:agent.keep_busy_policy` on every connect and on every change, across pods. No per-session `Request` field (§3f). Forwarding to a supervised Mac's worker moves to P3. Deferred to P5b: the heartbeat brief, the device-list badge, the audit | the org switch (default allowed) | #1952, merged `19b1f9fc8`; P5b #1965, merged `fcccec0f5`: the brief + the device-list chip |
| P6 | Docs: `docs/keep-busy.md` (mermaid state machine + sequence), a cross-ref from `docs/remote-control.md` §6, the `docs/README.md` row, the configuration reference; the field log | — | #1963, merged `19a9df753`; the field log waits for a release candidate |
| P7 | **A permanent history** (operator, 2026-10-11): `keep_busy_events`, owned by `fleet`, **no TTL**. Server rows (`org_denied` · `org_allowed` · `org_default_pattern`, only on a change, naming the admin) and device rows (`on` · `off`, derived on the heartbeat path from the previous stored brief against the new one — ONE `find_one_and_update` returning the row as it was — with a restart dedupe); `source` tells the server's decision from the device's claim. `GET …/keep-busy-history` (`MANAGE_AGENTS`, tenant + device scoped, `before`/`limit` paging); a History list with a device filter in the org's card. Resolution is the 30 s heartbeat; exact device-pushed edges would need an agent release | none (a record, not a gate; the route alone) | #1977 open |
| P8 | **The default pattern**: circle stays the product default; `TenantSettings.keep_busy_default_pattern` lets an org choose another, validated by **equality** against the agent's 13 wire names, `null` clears, absent leaves alone; the settings `GET` becomes readable by any member so the viewer can pre-select it (`PUT` stays `MANAGE_TENANT`). No agent and no wire change: the menu pre-selects the org default only where the person has no remembered choice; a fetch failure means circle | unset the default (= circle) | #1977 open |
| P9 | **GNOME on Wayland** (and GNOME on Xorg under a root daemon) through the arbiter's uinput device: a normalised move (`KbOp::MoveNorm`), Mutter's idle clock and layout asked by `portal-helper --keep-busy` inside the person's session, a **blind** host (`Host::blind`). Plus the GDM/Xorg lock fix found in the field: `logind::pick` | `uinput = false` (the GNOME host needs the uinput device); the cap word | #1978, merged `b2b715a50` |

## 5. Acceptance criteria

- [ ] **AC1:** turning it on in the viewer starts it on the host, and every connected viewer shows the
  same live state.
- [ ] **AC2:** while it runs, host idle time stays under 2 s on Windows, macOS and X11, measured. The
  negative control (OFF ⇒ the host locks and Teams goes Away) is recorded first.
- [ ] **AC3:** local input (move, click, key, scroll) and controller input each pause it within 1 s. It
  resumes after `resume_after`, and never while the screen is locked or a mouse button is held.
- [ ] **AC4:** it survives a disconnect, a daemon self-update and a reboot (resuming at the same
  person's next login).
- [ ] **AC5:** `keep_busy_enabled = false` hides and refuses it, and the server cannot change that key.
- [ ] **AC6:** an org deny refuses new enables and stops running instances, including on devices that
  reconnect later and across pods.
- [ ] **AC7:** the tray's Stop and `roomler keep-busy status|off` work, including through a Linux
  system install's person socket.
- [ ] **AC8:** no hook or raw-input API is used. A source-scan unit test passes (proven to fail on a
  planted line), the binary's import diff against the previous release is clean, and EDR stays quiet
  for 24 h on the corporate laptop.
- [ ] **AC9:** Wayland and portal-only hosts that keep busy cannot drive report `unavailable` with a
  reason the viewer shows (`wayland` without the uinput path; `no_idle_clock` on a desktop with no idle
  clock a helper can read).
- [ ] **AC13 (P9):** on GNOME on Wayland, through a system install with uinput on, keep busy runs:
  the negative control (OFF ⇒ GNOME locks at its idle delay) recorded first, then ON ⇒ no lock and
  Mutter's idle clock under 2 s, and a local key pauses it within 1 s.
- [ ] **AC10:** docs updated or created with mermaid diagrams (`docs/keep-busy.md`: the state machine,
  the sequence, the gates) and linked from `docs/README.md`; the configuration reference covers
  `keep_busy_enabled`.
- [ ] **AC11 (P7):** the history records every on and off a device reports (with who and the pattern,
  and the reason for an off) and every change to the org's settings (naming the admin, only on a
  change), survives the device's removal, is readable only with `MANAGE_AGENTS` and only within the
  tenant, pages by `before`, and is never aged out — field-verified on a release: a device turned on
  and off from the viewer shows two rows with the right names within one heartbeat.
- [ ] **AC12 (P8):** an org's default pattern is what a viewer's menu pre-selects on a device the
  person has no remembered choice on; a person's own last choice wins over it; `null` clears it to
  circle; an unknown pattern is refused; a plain member can read the setting and cannot set it.

## 6. Open decisions

- **Whether the arbiter should stop caching a `NoopInjector`.** That would harden RC input as well, but
  it changes RC behaviour and needs its own test and field check.

**Resolved 2026-10-11 (operator: "do permanent history, default pattern and wayland support"):**
- *The default pattern* — circle stays the product default; an org may choose another (P8,
  `keep_busy_default_pattern`). `subtle` was not made the default: the visible motion was what
  was asked for.
- *A durable audit of keep-busy transitions* — P7: its own collection, `keep_busy_events`, with
  **no retention** (permanent), the index-plan change and the composition re-baseline it needed,
  derived from the heartbeat briefs on the server (so no agent release) at the heartbeat's
  resolution. The agent's log keeps recording every on/off locally as before.
- *When Wayland ships* — P9: GNOME's Mutter IdleMonitor plus the arbiter's uinput device, blind (it
  cannot read the pointer back). KDE and wlroots have no idle clock a session helper can read, and
  report `no_idle_clock`. Coordinated with FR-93 (#1972), which owns making uinput the default on
  Wayland and the one desktop-session resolver.

## 7. Out of scope

- Sleep and power assertions (FR-55).
- Clicks, keys or scrolling of any kind.
- Enabling keep-busy without a consented, `INPUT`-granted session. A server-side enable would be the
  only server-forgeable input path in the product, so there is none.
- Work-hour schedules.
- A per-device admin deny in `AccessPolicy`.

## 8. Rejected along the way

- **Low-level hooks or raw input to tell real from synthetic input.** They're exact (`LLMHF_INJECTED`,
  a null `hDevice`), but EDR reads them as keylogging.
- **Cursor warps** (`SetCursorPos`, `CGWarpMouseCursorPosition`, `XWarpPointer`). They move the
  pointer without resetting the idle clock, which defeats the purpose.
- **Moving through `InputMsg::MouseMove` and `to_pixels`.** It reaches the primary display only, and it
  spends the per-session input diagnostics.
- **State "beside the config" or inside `config.toml`.** The hardened machine-global dir is not
  writable by the default user worker, and `config.toml` has several writers.
- **Device-wide state.** It would start moving a different person's mouse after they sign in.
- **A per-session org flag, alone or beside the push.** Alone, it cannot stop an instance that
  outlived its session, so the standing push exists. Beside the push it is redundant: the deny is
  device-wide (strictest of every enrolled org), and the engine refuses every enable while it holds.

## 9. Field-verification log

**2026-10-11, prod** (server `hosted-20261011-967e5b6`, agents **0.4.125**), on throwaway vmtest VMs
on zeus enrolled in the vmtest org, never a machine someone uses. The viewer is the production SPA
driven by Playwright as the org's admin; every `rc:keep-busy.state` frame is logged on the same
clock as the injected input. **Local input is real**: `virsh send-key` and QEMU's `mouse_move` /
`mouse_button` reach the guest through its virtual keyboard and mouse. Idle is the host's own clock,
read inside the session: `GetLastInputInfo` (an interactive scheduled task), `xprintidle`, and
Mutter's `GetIdletime` over the person's bus.

| # | Cell | Host | Result |
|---|---|---|---|
| 1 | AC2 negative control | Win11, system install (SystemContext worker), machine inactivity limit 60 s | **OFF: idle grew 0 → 100 s and the session LOCKED at 66 s** (LogonUI) |
| 2 | AC3: never while locked | same, locked | turned ON while locked: `locked` in 37 ms, and it stayed there |
| 3 | AC4: reboot | same, hard reset | after autologon: `running (circle), turned on by vmtest admin`, no viewer connected |
| 4 | AC2 ON | same, the 60 s lock armed | **15 min: 894 samples, idle max 47 ms (p99 32 ms), no lock** |
| 5 | AC3: local and controller input | same, `resume_after` 10 s | local mouse 0.83 s · controller mouse 0.27 s · controller key 12 ms · each resumed after 10.1 s |
| 6 | AC3: a single local key | same | **8 of 9 isolated Shift taps** paused it (0.5–1.1 s, including ~0.5 s of injection path); one slipped through inside a `GetLastInputInfo` tick shared with an own move (`docs/keep-busy.md` §10). Bursts of four taps: 4/4 in 0.40–0.50 s |
| 7 | AC3: a held button | same, QEMU `mouse_button 1` for 25 s | paused 0.81 s after the press, `button_held` when `resume_after` ran out, resumed 10.1 s after the release |
| 8 | AC2 negative control | Ubuntu 24.04, GNOME on Xorg, root daemon given the display (`/etc/default/roomlerd`), uinput off | **OFF: X idle 123 s, GNOME locked** (`LockedHint=yes`) |
| 9 | **a bug** | same, locked | turned ON while locked: **`running` behind GNOME's lock screen** (X idle 5 ms). GDM's Xorg session names no display to logind, so `logind::pick` matched nothing and the lock was never read. **Fixed in P9** |
| 10 | AC2 ON | same, unlocked, GNOME's 60 s lock armed | **5 min: 301 samples, X idle max 33 ms, no lock** |
| 11 | AC9 | Ubuntu 24.04 GNOME **Wayland**, system install, uinput on | `unavailable`, reason **`unsupported`** ("Unavailable on this computer."), not `wayland`: a root daemon has no `DISPLAY`, so the X11 host never even saw Xwayland. P9 makes this host run |
| 12 | AC13 negative control | same, keep busy unavailable | **OFF: Mutter idle reached 60 s and GNOME locked** |
| 13 | AC6: org deny, **across pods** | all three VMs' sockets on pod A; the PUT sent straight to pod B | every device logged `org policy received denied=true` and turned off, 17–38 ms after the PUT |
| 14 | AC6: refuses new enables | Win11, denied | the viewer greys the switch ("Disabled by your organization."); a raw `rc:keep-busy.set on` sent past it, straight down the control channel, came back `on=false org_denied` in 10 ms |
| 15 | AC6: a device that reconnects | Ubuntu (X11), denied, `roomler restart` | the policy arrived 5 ms after the new hello, and it stayed off. The re-allow (via pod B again) reached all three in ~20 ms and turned nothing back on |
| 16 | AC7: the CLI | Win11, over the guest's own shell | `roomler keep-busy status` (and `--json`) read the live state; `roomler keep-busy off` turned it off, "Turned off at this computer." |
| 17 | AC8: imports | the release MSIs, `roomlerd.exe` 0.4.124 → 0.4.125 | **exactly two new imports**, `user32!GetLastInputInfo` and `user32!SystemParametersInfoW`; no hook, raw-input or keyboard-state API added (`GetAsyncKeyState`/`GetKeyState` predate keep busy) |
| 18 | AC13 ON (**P9 build**) | Ubuntu 24.04 GNOME **Wayland**, system install, uinput on, GNOME's 60 s lock armed | the GNOME host came up 44 ms after the turn-on (`idle_clock=true geometry=true`), `detector` `clock`; **~5 min: 284 samples, Mutter idle max 46 ms (p99 33 ms), no lock** |
| 19 | AC13: local input, lock (P9) | same | a key paused it in 0.64 s, a move of the VM's USB tablet in 0.54 s; each resumed after ~10 s. `loginctl lock-session` ⇒ `locked`; the unlock ⇒ paused for whoever unlocked, then running |
| 20 | **the fix** (P9) | the Xorg host of cell 9, P9 installed | turned ON while locked: **`locked` in 229 ms, X idle growing** — the same cell 0.4.125 failed |
| 21 | AC1: every viewer | two viewers on the GNOME Wayland guest, the second one with the floor | the floor holder switched it on; **the other viewer, doing nothing, saw it 13 ms later**, both toolbars "Keep busy · locked". The off reached the other in 15 ms (`stopped_by_controller`) |
| 22 | AC5: the owner's off | the GNOME Wayland guest, running | `roomler config set keep_busy_enabled false` applied live: it stopped (`device_disabled`, "Turned off on this computer"), the `keep-busy` word left the server's listing within a heartbeat, and a raw `set on` was refused `device_disabled`. A connected viewer shows the control **disabled with the owner's reason** (the P2 design: `agentSupportsKeepBusy \|\| state`), not hidden |
| 23 | AC7: a Linux system install's person socket | the GNOME Wayland guest, as the unprivileged user | `roomler keep-busy status` read the root daemon's state and `roomler keep-busy off` stopped it ("Turned off at this computer.") |
| 24 | Worker types: the user-context worker | Win11 attended install (`worker role probed role=User`) | on ⇒ running in 228 ms; **69 samples before the touch, session idle max 32 ms**; a real local key paused it ~1 s later, and it resumed after 10.1 s. The store is the person's own `%LOCALAPPDATA%\roomler\roomler\data\keep-busy.json` (its owner reads `BUILTIN\Administrators`: the default owner of the worker's elevated token; access is the profile's inherited ACL) |
| 25 | AC12 (P8), **prod** `hosted-20261011-a5429ac` | the vmtest org | the admin set the org default to `heart` (200). A FRESH viewer, with nothing remembered for the device, pre-selected `["heart"]`. Reset to `null` afterwards |
| 26 | AC11 (P7), **prod** | the GNOME Wayland guest | on from the viewer at 09:37:21 ⇒ an `on` row (device, `vmtest admin`, circle) at 09:37:50, the next heartbeat; off at 09:39:08 ⇒ an `off` row (`stopped_by_controller`) at 09:39:20. The org-wide list, newest first: `off` · `org_default_pattern` → null (server, the admin) · `on` · `org_default_pattern` heart (server, the admin) — exactly the four changes made, nothing else |

Wrong turns, recorded so nobody repeats them:
- **An ephemeral (vmtest) device unenrolls on a clean OS restart.** Windows' servicing
  (TrustedInstaller, "Operating System: Upgrade") restarted the guest once after a hard reset, and
  the device removed itself, by design (FR-51). The reboot cell needs a standard enrollment.
- The machine inactivity limit does not arm until the next boot; a key after setting it is not
  enough.
- **A "missed" local mouse on the Ubuntu VM was the test's.** HMP `mouse_move` is relative, and that
  guest's active mouse is QEMU's absolute USB tablet (its PS/2 mouse is vmmouse, absolute too), so
  nothing reached GNOME. A QMP `input-send-event` move of the tablet paused it in 0.54 s.
- The P9 test binary was built locally with `RUSTFLAGS=-A dead_code`: without `ffmpeg-encoder` a
  release build hits rustc 1.95's ICE in the dead-code lint (`check_mod_deathness`, modules
  `clipboard` and `encode::policy`). CI's lines carry the feature or the flag.

Still owed: macOS (cell 6 of `docs/keep-busy.md` §14), the tray's Stop (AC7), a self-update (AC4,
the next release), AC9/AC13 on a release (P9 rides 0.4.126), and 24 h of EDR on the corporate
laptop (AC8). AC11 and AC12 are proven on prod (rows 25–26).

Two criteria need the operator's wording, not more testing:
- **AC3** says "local input (… key …) pauses it within 1 s". An isolated single key tap can share a
  `GetLastInputInfo` tick with an own move and is then invisible on Windows (1 of 9 in the field);
  typing, clicks, the mouse and a held button are always caught. Accept it as documented, or reword.
- **AC5** says "hides". The device withdraws the cap word (every surface that reads it hides keep
  busy), but a connected viewer shows the control disabled with the owner's reason — by the P2
  design, so a controller is told why rather than finding it gone.

## 10. Related

- FR-55: keep-busy overrides `power_policy = never` (§3f).
- FR-27, FR-85, FR-43, and the P6 arbiter (`input/arbiter.rs`).
- #1738: the lock monitor is session-scoped.
- **Found along the way, filed separately:** the arbiter's release-all on session close sends a
  button-up at (0,0), and dispatch moves first. So a session that closes mid-drag throws the cursor
  into the top-left corner, a hot corner on GNOME and macOS (`arbiter.rs` L109-118,
  `enigo_backend.rs` L128-138).
