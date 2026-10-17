# FR-109: Relative mouse — Pointer Lock in the browser for apps that capture the pointer

**Issue:** [#2333](https://github.com/gjovanov/roomler-ai/issues/2333) · **Status:** in progress — P1 built
(Windows; [`docs/relative-mouse.md`](../relative-mouse.md)) · **Owner:** the agent's input path (`input/`, every injector), the viewer (Pointer Lock, the input
pump) · **Anchors:** master `7382639b8` · **Program:** [StarDesk teardown](../plans/stardesk-teardown-program.md),
wave 3 (§6.1, row P9) · **Builds on:** [FR-92](FR-92-keep-busy.md) (the agent never reads input), #1945
(the arbiter's release-all), FR-98 C3 (the input gap)

## 1. Goal

A controller in the browser can drive what reads **relative** mouse motion: a 3D or CAD viewport, a game
camera, anything that hides the cursor and recentres it every frame. StarDesk calls this "Client mouse"
(the teardown's row P9). Roomler has only the absolute mouse. When the host app captures the pointer, the
controller's cursor reaches the edge of the stage and the motion stops, while the app sees a pointer
pinned at the centre.

Nothing to install on the controlling side stays true: Pointer Lock is a web API.

## 2. Evidence (master `7382639b8`)

| What | Where |
|---|---|
| Every move is absolute: `mouse_move {x, y, mon}`, normalised `0..1` per monitor, so a resolution change mid-session needs no resync | [`input/mod.rs:191`](../../agents/roomlerd/src/input/mod.rs#L191) `InputMsg` |
| enigo moves to an absolute pixel | [`enigo_backend.rs:132`](../../agents/roomlerd/src/input/enigo_backend.rs#L132) `Coordinate::Abs` |
| The uinput device is an absolute tablet **by design**: "a relative device would force us to track the pointer and guess at acceleration" | [`uinput_backend.rs:18`](../../agents/roomlerd/src/input/uinput_backend.rs#L18), axes at `:298` |
| The portal path moves with `NotifyPointerMotionAbsolute` on the stream | [`portal/input.rs:449`](../../agents/roomlerd/src/capture/portal/input.rs#L449) |
| The viewer coalesces pointer moves at the input gap and sends the last position | [`useRemoteControl.ts:5862`](../../ui/src/composables/useRemoteControl.ts#L5862), the gap's key at `:2173` |
| The agent already says when the host hides its cursor (`cursor:hide`); the viewer only drops its ghost cursor | `useRemoteControl.ts:9160` |
| No Pointer Lock anywhere in the viewer | `git grep requestPointerLock ui/src`: nothing |
| `AgentCaps.input` today: `arbiter`, `exclusive`, `ghost-cursor` | [`caps.rs:1460`](../../agents/roomlerd/src/encode/caps.rs#L1460) |

## 3. Design

```mermaid
sequenceDiagram
    participant U as Controller (browser)
    participant V as Viewer
    participant A as roomlerd (input arbiter)
    participant O as Host OS / app
    A-->>V: hello: AgentCaps.input has "mouse-rel"
    U->>V: toggle "relative mouse" (a gesture)
    V->>V: requestPointerLock({unadjustedMovement: true})
    loop every input gap
        U->>V: movementX/Y (summed, the fraction carried)
        V->>A: {t:"mouse_rel", dx, dy}
        A->>A: the holder only (arbiter)
        A->>O: SendInput relative / XTest relative / uinput REL_X,REL_Y / portal NotifyPointerMotion
    end
    U->>V: Esc (the browser's own rule)
    V->>V: pointer unlocked, absolute moves again
```

### 3a. The wire

- `InputMsg::MouseRel { dx: i32, dy: i32 }`, `t: "mouse_rel"`, in whole counts. The viewer sums
  `movementX/Y` across the input gap and carries the fraction into the next message, so slow motion is not
  rounded away.
- It is a **real variant**, like #1945's `MouseRelease`: every injector's match is exhaustive, so each
  backend has to decide what it does with it, and none can forget.
- **A click must not move the pointer.** `mouse_button` carries a position, and every backend moves there
  before the button (`input/mod.rs`, `MouseButton`). In relative mode the viewer has no position to send:
  the host app has hidden the cursor and owns where it is. So relative mode presses with a second new
  variant, `InputMsg::MousePress { btn }`, which presses where the pointer is. It releases with #1945's
  existing `MouseRelease { btn }`, which already lets go without moving. The wheel carries no position and
  needs nothing.
- ⚠️ An older agent fails to parse a variant it does not know, so the frame is **gated at both ends**:
  - the agent advertises `mouse-rel` in `AgentCaps.input` only where its injector can carry it;
  - the viewer sends `mouse_rel` only to an agent that advertises it;
  - the match is equality, and an unknown word is ignored (the `RpcCap` rule).

### 3b. Injection, per backend

| Backend | Relative motion | P0 checks |
|---|---|---|
| enigo, Windows (user worker) | `Coordinate::Rel` → `SendInput` with `MOUSEEVENTF_MOVE` and no `ABSOLUTE` | a raw-input reader sees the injected deltas; what pointer acceleration does to the cursor and to raw input |
| SystemContext (SYSTEM in the console session) | the same enigo, on its dedicated thread ([`system_context_backend.rs:5`](../../agents/roomlerd/src/input/system_context_backend.rs#L5)) | the same, from that worker |
| enigo, X11 | XTest relative motion | an XInput2 raw reader sees them |
| enigo, macOS | enigo's relative move | whether apps read the deltas, or a `CGEvent` needs the delta fields set (open decision 2) |
| uinput (system installs) | a **second**, relative-only device (`REL_X`, `REL_Y`), made on first use. The absolute tablet stays as it is | how libinput classifies it beside the tablet, on GNOME and KDE |
| the Wayland portal | `NotifyPointerMotion(dx, dy)`, which needs no stream | that GNOME and KDE honour it (open decision 3) |

### 3c. The arbiter

`mouse_rel` is movement. Only the session holding input injects it, the same as `mouse_move`.
Held-button tracking, the modifier fence and release-all on teardown are unchanged
([`arbiter.rs:17`](../../agents/roomlerd/src/input/arbiter.rs#L17)): a drag in relative mode is a held
button plus deltas.

### 3d. The viewer

- **Entering:** a toolbar toggle, shown only when the agent advertises `mouse-rel`. Pointer Lock needs a
  user gesture, and the toggle is one. `requestPointerLock({unadjustedMovement: true})` asks for raw,
  unaccelerated deltas. A browser that rejects the option gets plain Pointer Lock.
- **Leaving:** Esc, the browser's own rule, or the toggle. With Keyboard Lock active in fullscreen, Esc
  must be held. The viewer says so in its hint.
- **While locked:** `mousemove` carries `movementX/Y`; buttons and the wheel keep their messages. The
  ghost cursor is hidden.
- **Coalescing:** at the existing input gap (FR-98 C3), summing deltas instead of keeping the last
  position.

### 3e. Auto-switch (P3)

`cursor:hide` already arrives when the host hides its cursor. In relative-capable sessions it offers
relative mode: the next click inside the stage enters Pointer Lock (open decision 1), and the cursor coming
back (`cursor:shape`) leaves it.

⚠️ The lock screen and fullscreen video hide the cursor too. So the switch waits for a click, and **never**
happens while the host is locked.

### 3f. What the agent never does

The agent only injects. It never reads input, so there is no hook, no raw-input sink and no key polling, by
FR-92's rule. The field probes that read raw input (§5) are test scripts on throwaway guests, never part
of `roomlerd`.

### 3g. As built (P1)

- **The wire:** `MouseRel { dx: i32, dy: i32 }` and `MousePress { btn }`. A fractional `dx` does not parse,
  so the agent drops it as it drops any unreadable frame.
- **The gate:** `advertises_mouse_rel()` is true on a Windows build with `enigo-input` unless
  `ROOMLERD_INPUT_MOUSE_REL` says a false word, and `stub_caps` appends the word at every announcement,
  beside `keep-busy`. The viewer offers the control on equality, unless the browser's
  `roomler-rc-relative-mouse` is `off`.
- **Windows** needed one change for both injectors: enigo's shared `dispatch`, which the SystemContext
  worker reuses through `dispatch_for_external`.
- **The arbiter** tracks `MousePress` as held. Its tracking ended in a `_ => {}` arm, where the new press
  would have fallen through, and a session ending mid-drag would never have let go.
- **Leaving Pointer Lock** sends the gathered motion and releases held buttons with `mouse_release`. The
  blur release uses `mouse_release` too in relative mode: the old one sent `mouse_button` at the last
  absolute point, which would throw a captured pointer there.
- `docs/relative-mouse.md` has the tables and the anchors.

## 4. Phases

| P | Scope | Kill switch | Status |
|---|---|---|---|
| P0 | This spec and the claim. Probes: (1) Windows `SendInput` relative from the user worker and from SystemContext, read by a raw-input probe that hides and recentres the cursor, with acceleration on and off; (2) X11 XTest relative; (3) a relative uinput device beside the tablet, on GNOME and KDE; (4) the portal's `NotifyPointerMotion`; (5) the browsers: Pointer Lock, `unadjustedMovement`, Esc under Keyboard Lock | docs only | spec and claim 2026-10-16 (#2334). **(1), (2) and (5) measured** (§8 rows 1, 2, 4): relative `SendInput` and relative XTest both reach a raw reader 1:1, absolute moves give it nothing on either, and every headless engine rejects `unadjustedMovement`. Owed: (1) from SystemContext (AC1's green), (3) uinput beside the tablet, (4) the portal, which decide the rest of P2 |
| P1 | The wire, the gate, enigo's relative path (Windows user and SystemContext), the arbiter, the viewer's toggle and pump | agent `ROOMLERD_INPUT_MOUSE_REL=0` (stops advertising `mouse-rel`); viewer `roomler-rc-relative-mouse=off` | **built** (3g): `mouse_rel` and `mouse_press` in every backend's match. Windows advertises `mouse-rel`; Linux and macOS do not until P2, and their backends press where the pointer is but drop relative motion. Unit tests: wire spellings, the kill switch, enigo's calls, the arbiter, uinput's events, the portal plan, and the viewer's accumulator and gate (vitest 2153). Negative controls, each of which failed its test: a prefix-matched cap, rounding in the accumulator (it passed at first, so the test was tightened), a dropped carry, an untracked press, and a press that moves first. Field green owed: AC1, AC3, AC4 on the agent release that carries it |
| P2 | uinput's relative device; the portal and macOS by P0's answers | the same | — |
| P3 | Auto-switch on `cursor:hide` | viewer storage key | — |
| P4 | Docs: `docs/relative-mouse.md`, linked from `docs/remote-control.md` §6 and indexed in `docs/README.md` | — | the page exists from P1 and grows with each phase |

## 5. Acceptance criteria

- [ ] **AC1 (Windows, red first):** on a vmtest Win11 host, a probe script hides the cursor, recentres it
  and sums raw-input deltas. The controller moves 1000 counts right. Red: today the probe sees the motion
  stop where the controller's cursor reaches the stage edge. Green: in relative mode the probe's sum is
  within 5 % of the controller's, on the user worker and on SystemContext.
- [ ] **AC2 (Linux):** the same on an X11 host (XTest), and on a system install through uinput. The
  absolute pointer keeps working beside it in both.
- [ ] **AC3 (the gate):** an agent without `mouse-rel` never receives `mouse_rel`, and its viewer shows no
  toggle. An older viewer with a P1 agent works as today.
- [ ] **AC4 (the arbiter):** a watcher who does not hold input cannot move the pointer relatively, and a
  session that ends mid-drag still releases its button.
- [ ] **AC5 (auto-switch):** a hidden cursor offers relative mode on the next click; a locked host never
  enters it.
- [ ] **AC6 (no hooks):** a source scan in FR-92's shape covers this FR's agent code and fails on a planted
  raw-input registration.
- [ ] **AC7:** **Docs:** a `docs/relative-mouse.md` page (mermaid, the per-OS table, the limits), linked
  from `docs/remote-control.md` §6, with a row in `docs/README.md`'s index.

## 6. Open decisions

1. **Auto-switch.** Enter Pointer Lock on the next click while the host's cursor is hidden, or only offer
   it. Recommended: enter, with a visible "Esc releases the mouse" hint. The click is the gesture Pointer
   Lock needs anyway.
2. **macOS hosts.** enigo's relative move, or a `CGEvent` with its delta fields set explicitly. Recommended:
   let P0 decide by what a 3D app actually reads.
3. **The Wayland portal.** Relative motion through `NotifyPointerMotion`, or refused by name with a reason
   the viewer shows. Recommended: carry it if P0 shows GNOME and KDE honour it.
4. **The kill switch.** Recommended: the env var `ROOMLERD_INPUT_MOUSE_REL=0`, read once where the caps are
   built, so a device stops advertising and no viewer sends the frame; plus a viewer storage key.

## 7. Out of scope

- Gamepad passthrough (excluded by the operator).
- Touch presets and macros (the next wave-3 FR).
- Reading input on the host in any form (§3f).
- Changing the host's pointer speed or acceleration settings.

## 8. Field-verification log

| # | Date | Cell | Result |
|---|---|---|---|
| 1 | 2026-10-16 | **P0 (1): `SendInput` and a raw-input reader, no agent involved.** A kept vmtest Win11 guest with `vmtest` Active at the console. A test-only probe, run as the person through an interactive task, reads raw mouse input (`RIDEV_INPUTSINK`), sums relative deltas, counts absolute events and recentres the cursor after every move. A second task, also as the person, sends 10 moves of +100 with `SendInput`, relative (`MOUSEEVENTF_MOVE`, what enigo's `Coordinate::Rel` sends) or absolute (`MOVE\|ABSOLUTE`, what every viewer move becomes today) | **Relative arrives 1:1; absolute gives a raw reader nothing.** Relative and recentred: sum **1000**, 10 relative events. Absolute and recentred: **0** relative, 10 absolute events. Relative without recentring: 1000. So pointer acceleration does not touch the raw deltas, and recentring with `SetCursorPos` adds no raw motion. The same from SystemContext is AC1's green |
| 2 | 2026-10-16 | **P0 (5): Pointer Lock in Playwright's headless engines** (image v1.62). A local page asks for Pointer Lock on a click, `{unadjustedMovement: true}` first and plain if rejected, then gets 10 synthetic moves of +10 px | **Every engine rejects `unadjustedMovement`.** Chromium 151 and Firefox 153 then lock with plain Pointer Lock; WebKit 26.5 cannot lock ("requires the window to have focus"). The synthetic `movementX` is unusable: Chromium sums to (−200, −150) over 84 events, Firefox to (−3850, −2100). So P1's fallback to plain Pointer Lock is load-bearing, and AC1's cell sends exact `mouse_rel` frames on the input channel instead of measuring through headless Pointer Lock |
| 3 | 2026-10-16 | **AC1 and AC3, the red (0.4.139).** The kept guest of row 1 on agent 0.4.139; prod at `hosted-20261016-377f1d4`; row 1's probe sums raw input for 150 s while the production viewer, driven by Playwright from the vmtest harness host, holds a session (`rel-ac1.sh`) | **Nothing relative reaches the host today.** `red-abs`, the viewer's own 10 absolute moves: the probe sums **0** relative and counts 20 absolute events. `red-rel`, 10 × `mouse_rel {dx: 100}` sent raw on the input channel: **0** and 0, because the old agent cannot parse the frame and drops it safely. The agent advertised `input = ["arbiter", "exclusive", "ghost-cursor", "keep-busy"]`, with no `mouse-rel`, and the viewer showed no relative-mouse control. The green is owed on 0.4.140, which carries P1 (#2337) |
| 4 | 2026-10-17 | **P0 (2): XTest relative on X11, no agent involved.** Xvfb (X.Org 21.1) on a dev box; `xinput test-xi2 --root` records XI2 events; `xdotool` sends 10 moves of +100 through XTest, relative (`mousemove_relative`, the same `XTestFakeRelativeMotionEvent` enigo's X11 `Coordinate::Rel` uses) or absolute | **The same picture as Windows.** Relative: **10** RawMotion events, raw dx summing to exactly **1000.0**. Absolute: **0** RawMotion events (10 plain Motion events). So an X11 game that reads XI2 raw motion gets relative moves 1:1 and nothing from absolute ones. P2 can advertise `mouse-rel` on X11 sessions whose injector is enigo. A real Xorg with a compositor, and Xwayland, are not measured |

## 9. Related

- [Plan §6.1](../plans/stardesk-teardown-program.md): this FR, and its place in wave 3.
- [FR-92](FR-92-keep-busy.md): the no-hooks rule.
- #1945: `MouseRelease`, the arbiter's release-all without a move.
- FR-98 C3: the input gap the pump coalesces at.
