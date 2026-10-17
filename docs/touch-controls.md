# Touch controls — a phone or tablet as a real controller

> **Status** (FR-111, [#2359](https://github.com/gjovanov/roomler-ai/issues/2359); spec:
> [`docs/fr/FR-111-touch-controls.md`](fr/FR-111-touch-controls.md))
>
> | Phase | What | State |
> |---|---|---|
> | P1 | touch on the stage: it keeps every touch (`touch-action: none`), a cancelled pointer still lets go, and the gestures of direct mode (the right button, two-finger scroll, pinch zooming only the viewer) | built (viewer only) |
> | P2 | a trackpad mode: the finger moves the host's pointer, a tap clicks where it is | built (viewer only). With nothing remembered, a phone-sized screen starts in trackpad mode and a larger one in direct mode (the operator's decision 1, 2026-10-17) |
> | P3 | on-screen controls: rows of keys, a d-pad, a mouse stick, in three built-in presets remembered per device | built (viewer only); the presets follow the recommendation until the operator decides |
> | P4 | an editor, macros, tenant presets | **P4a built**: the org's presets on the server (four routes, strict validation, macros in the schema). P4b, the editor and the overlay reading them, not built |

## Why it exists

The controller is the browser, so a phone or a tablet is a controller with nothing to install. Until FR-111,
the viewer treated a finger as a mouse: a touch moved the host's pointer to that point and pressed the left
button there. It offered nothing else a touch screen needs, and it got one thing wrong:

**A drag the browser took over left the host's button down.** A touch browser decides for itself whether a
moving finger is a pan or a zoom. When it decides so, it ends the pointer with `pointercancel` and never
sends `pointerup`. The viewer had already forwarded the press, had no `pointercancel` handler, and so never
sent the release. On a vmtest Win11 host the left button stayed down until a later tap let go, and in the
meantime every touch moved into a drag. FR-111's P0 measured it: the browser cancelled 69 ms into the drag,
and the host's raw input read the press and then no release (the spec's §8 row 1).

## The gestures (direct mode)

The finger is still the mouse: a tap clicks where it lands, and a drag drags from where it landed. What is new
is that the viewer **waits for the finger to say what it is** before it presses anything. So a long press
never starts as a left press, and a second finger never drags along with the first
([`touchGestures.ts:168`](../ui/src/composables/touchGestures.ts#L168) `createDirectGestures`, a pure state
machine with its own tests).

| Gesture | Reaches the host as |
|---|---|
| a tap | a left click where the finger landed (a lifting finger jitters, so not where it lifted) |
| a finger that moves more than 8 px | a left press where it landed, the moves, a release where it lifted ([`touchGestures.ts:37`](../ui/src/composables/touchGestures.ts#L37) `SLOP_PX`) |
| a finger held still for 500 ms | a right click where it is ([`touchGestures.ts:39`](../ui/src/composables/touchGestures.ts#L39) `LONG_PRESS_MS`) |
| two fingers lifted within 350 ms, unmoved | a right click between them ([`touchGestures.ts:42`](../ui/src/composables/touchGestures.ts#L42)) |
| two fingers moving together | the wheel, one notch per 100 px the fingers travel. The picture follows the fingers, so fingers moving up scroll down. **While the picture is zoomed past the stage**, they pan the picture instead and a one-line hint says so; the host scrolls only once the picture fits (the operator's decision 4, 2026-10-17; [`RemoteControl.vue:3247`](../ui/src/views/remote/RemoteControl.vue#L3247) `onTwoFingerScroll`, [`touchInput.ts:104`](../ui/src/composables/touchInput.ts#L104) `pictureOverflows`) |
| two fingers spreading or pinching | **nothing**: the viewer zooms its own picture around the point between the fingers |

```mermaid
stateDiagram-v2
    [*] --> Pending: a finger lands on the picture
    Pending --> Dragging: moves past 8 px (left press where it landed)
    Pending --> Clicked: lifts (left click)
    Pending --> Held: 500 ms still (right click)
    Pending --> Two: a second finger lands
    Two --> Clicked: both lift within 350 ms, unmoved (right click)
    Two --> Scrolling: they move together (wheel)
    Two --> Zooming: their distance changes 24 px (viewer zoom)
    Dragging --> [*]: lifts (release) or the browser cancels (release at the last point)
    Clicked --> [*]
    Held --> [*]
    Scrolling --> [*]
    Zooming --> [*]
```

- **The zoom** switches the viewer to Custom zoom, starting from the size the picture is shown at, and scrolls
  the stage so the point between the fingers stays under them
  ([`RemoteControl.vue:3219`](../ui/src/views/remote/RemoteControl.vue#L3219) `onPinchZoom`). Zooming
  out and back in somewhere else is how a phone moves around a zoomed picture today (open decision 4 in the
  spec).
- ⚠️ **The scroll goes out in whole steps.** The agent turns a `mouse_wheel` in pixels into wheel steps at
  100 px a step and rounds every frame on its own (`enigo_backend.rs` `wheel_to_steps`), so a frame under
  50 px scrolls nothing, and a finger's scroll arrives a few pixels at a time. The viewer gathers the
  gesture's pixels and sends only whole steps, carrying the rest
  ([`touchGestures.ts:147`](../ui/src/composables/touchGestures.ts#L147) `createWheelSteps`). On the
  field run that turned 21 frames of about 6 px, which the host never saw, into one step it did.
- A mouse and a pen keep their own paths. Only a pointer whose `pointerType` is `touch` goes through the
  gestures ([`useRemoteControl.ts:10441`](../ui/src/composables/useRemoteControl.ts#L10441)).
- Every gesture is made of frames a mouse already sends (`mouse_button`, `mouse_move`, `mouse_wheel`), so
  it works with every deployed agent.

## Trackpad mode

On a phone the finger covers what it points at. In trackpad mode the screen is a laptop's trackpad instead.
The finger **moves** the host's pointer, which stays where it is between touches, and a tap clicks where the
pointer is, not under the finger. A toolbar control switches between direct and trackpad, shown only on a
device with touch and remembered per browser (`localStorage['roomler-rc-touch-mode']`,
[`touchInput.ts:90`](../ui/src/composables/touchInput.ts#L90) `readTouchMode`). **With nothing remembered, a
phone-sized screen starts in trackpad mode** and a larger one in direct mode, the operator's decision 1
(2026-10-17). A phone is a screen whose shorter side is under 600 CSS px, read from the screen and not the
window, so a small desktop window is not a phone ([`touchInput.ts:80`](../ui/src/composables/touchInput.ts#L80)
`phoneSized`). The toolbar's choice is remembered as the exact word and wins on any screen.

| Gesture (trackpad) | Reaches the host as |
|---|---|
| a finger that moves | the pointer moves by the same share of the screen as the finger moved across the picture |
| a tap | a left click where the pointer is |
| a finger held 500 ms, then moved | a left drag, let go when the finger lifts |
| two fingers | a quick tap is the right button and a pinch zooms the viewer, as in direct mode. Moving together is always the wheel, as on a laptop's trackpad: decision 4's pan of a zoomed picture is direct mode's |

([`touchGestures.ts:333`](../ui/src/composables/touchGestures.ts#L333) `createTrackpadGestures`, the same
two-finger handling as direct mode.)

- **Two ways to move the pointer**, picked by what the agent advertises
  ([`useRemoteControl.ts:10527`](../ui/src/composables/useRemoteControl.ts#L10527) `runTrackpad`):
  - **An agent that advertises `mouse-rel`** gets the travel as FR-109's `mouse_rel` and presses with
    `mouse_press` / `mouse_release`, which act where its pointer is.
  - **An older agent** gets absolute moves. The viewer starts from where the host's pointer is (its last
    `cursor:pos`, divided by the picture's size), adds the finger's travel, and presses at that point
    ([`useRemoteControl.ts:10511`](../ui/src/composables/useRemoteControl.ts#L10511) `hostPointerNorm`).
    It re-reads `cursor:pos` at the start of each gesture, so it follows a pointer that someone else moved.
- **The travel** is measured in the picture's own normalised space, the same mapping the stage's clicks use,
  so letterbox and zoom are already accounted for
  ([`useRemoteControl.ts:10519`](../ui/src/composables/useRemoteControl.ts#L10519) `padTravel`).
- The whole stage is the pad: in trackpad mode a finger may land on the letterbox too.

## On-screen controls

A phone has no keys for Esc, Tab, the arrows or a chord, and a game wants WASD under one thumb and the
mouse under the other. The **On-screen controls** menu in the toolbar, shown on a device with touch, lays
a preset over the stage: rows of keys, a d-pad, a mouse stick
([`TouchControls.vue`](../ui/src/components/remote/TouchControls.vue)). Every control sends frames every
deployed agent already takes: `key` for a key, and the mouse frames for a mouse button and for the stick.

| Preset | Controls |
|---|---|
| **Office keys** | one row along the bottom: Esc, Tab, Alt+Tab, Ctrl+C, Ctrl+V, Ctrl+Z |
| **Arrows** | an arrow d-pad bottom left; PgUp, PgDn, Space, Enter and Esc bottom right |
| **WASD + mouse look** | a W/A/S/D d-pad bottom left; a mouse stick bottom right, with Fire (the left button), Aim (the right button) and Jump (Space) above it; Esc, E, R, Shift and Ctrl along the top |

([`touchControls.ts:116`](../ui/src/composables/touchControls.ts#L116) `BUILTIN_PRESETS`. They follow the
spec's recommendation for open decision 2 until the operator settles it.)

```mermaid
flowchart LR
    F[A finger on a control] -->|captured, never reaches the stage| C[TouchControls.vue]
    C --> D[The driver: what each finger holds, every key counted]
    D -->|key, button, motion| S[The stage's sink]
    S -->|key frames| A[roomlerd]
    S -->|mouse_press, mouse_rel: an agent with mouse-rel| A
    S -->|mouse_button, mouse_move: an older agent| A
```

- **A touch on a control stays with it.** The control captures its pointer and stops its events, so the
  stage's gestures never see it. A touch anywhere else reaches the stage as before
  ([`TouchControls.vue:32`](../ui/src/components/remote/TouchControls.vue#L32)).
- **A button holds its keys** while the finger is down: a chord presses in order and lets go in reverse.
  **Every key and button is counted**, so two fingers holding one key send one press and one release
  ([`touchControls.ts:362`](../ui/src/composables/touchControls.ts#L362) `createControlsDriver`).
- **The d-pad** has eight sectors. Each direction covers 135° around its axis, so a diagonal holds two keys,
  and a finger near the centre holds none
  ([`touchControls.ts:287`](../ui/src/composables/touchControls.ts#L287) `dpadKeys`). A finger sliding
  round it lets go of a key it left before it presses one it reached.
- **The stick** moves the pointer while it is pushed, at a share of the picture's width a second. The speed
  grows with the square of the push past a deadzone, so a small push is precise
  ([`touchControls.ts:305`](../ui/src/composables/touchControls.ts#L305) `stickVelocity`). An agent that
  advertises `mouse-rel` gets the motion as `mouse_rel`, so a game camera turns. An older agent gets
  absolute moves from where its pointer is, as in trackpad mode. Fire and Aim press the same two ways.
- **Nothing stays held.** The overlay lets go of everything when the preset changes, the window blurs, the
  tab is hidden or the overlay closes
  ([`TouchControls.vue:227`](../ui/src/components/remote/TouchControls.vue#L227) `releaseAll`). The stage's
  own release on a blur or a detach covers the controls' presses too, and a release for something already
  let go is dropped ([`useRemoteControl.ts:10600`](../ui/src/composables/useRemoteControl.ts#L10600)
  `stageControls`).
- **Ctrl on a Mac host** goes out as the keyboard's would (FR-13): with the viewer turning Ctrl into Cmd,
  Ctrl+C reaches the Mac as Cmd+C, and the button says ⌘C
  ([`touchControls.ts:274`](../ui/src/composables/touchControls.ts#L274) `controlLabel`).
- **Per device, in this browser.** The choice is kept under
  `localStorage['roomler-rc-touch-controls:<device id>']`, so a host that runs a game keeps its WASD and a
  work laptop its office row
  ([`touchControls.ts:227`](../ui/src/composables/touchControls.ts#L227) `readControlsPreset`). The
  default is off.
- **In fullscreen and in a zoomed picture.** The overlay lives inside the stage, which is the fullscreen
  element, and follows the stage's scroll, so it stays over the part of the picture on screen
  ([`RemoteControl.vue:3203`](../ui/src/views/remote/RemoteControl.vue#L3203) `onStageScroll`).
- ⚠️ **Alt+Tab is the host's own chord.** On a Windows host it opens the task switcher and moves the focus,
  as the physical keys do. On a Mac host it is Option+Tab, not the app switcher (Cmd+Tab). A preset cannot
  bind a chord per host OS until P4's editor.

## The org's presets (P4)

An org can keep presets of its own beside the three built-ins: an admin saves one, and every member's
viewer offers it. **P4a, the server half, is built**. P4b (the editor, the presets in the overlay, macros
played by the viewer) is not yet, so nothing reads them today.

```mermaid
sequenceDiagram
    participant Ad as Org admin (MANAGE_TENANT)
    participant S as Server (remote module)
    participant DB as touch_presets
    participant M as A member's viewer
    Ad->>S: POST /api/tenant/{tid}/touch-preset {name, controls}
    S->>S: deny unknown fields, TouchPresetRequest::validate
    S->>DB: insert (unique per org by name)
    S-->>Ad: 201 the stored preset, or 409 / 422 with a code
    M->>S: GET /api/tenant/{tid}/touch-preset
    S-->>M: {items: [{id, name, controls, updated_at}]}
```

- **The data is the viewer's own preset**, stored as it is: rows of buttons, d-pads and sticks
  ([`models.rs:4868`](../crates/remote_control/src/models.rs#L4868) `TouchPreset`). P4 adds one action
  beside a chord of keys and a mouse button: a **macro**, a list of steps, each a key or a mouse button
  pressed, released or tapped, then a wait
  ([`models.rs:4986`](../crates/remote_control/src/models.rs#L4986) `MacroStep`).
- **Who:** any member reads them, since every viewer in the org offers them. Writing takes `MANAGE_TENANT`,
  like the org's session policy, because a stored preset runs on every member's viewer
  ([`touch_presets.rs:88`](../crates/modules/remote/src/touch_presets.rs#L88) `list`,
  [`touch_presets.rs:109`](../crates/modules/remote/src/touch_presets.rs#L109) `create`). That is open
  decision 5, built to its recommendation.
- **Resolved within the org.** A change or a delete looks the preset up in the org it names, so a foreign id
  is a 404 that leaks nothing
  ([`touch_presets.rs:152`](../crates/modules/remote/src/touch_presets.rs#L152) `update`,
  [`touch_presets.rs:185`](../crates/modules/remote/src/touch_presets.rs#L185) `delete`).
- **Read strictly** ([`models.rs:5040`](../crates/remote_control/src/models.rs#L5040) `validate`). Each
  refusal is a 422 with a code, and nothing is stored:

  | Refused | Code |
  |---|---|
  | an unknown field at any level, a key past a byte, a malformed body | `invalid_json` |
  | a key the keyboard path cannot send: only the Keyboard/Keypad page's keys and its eight modifiers ([`models.rs:5015`](../crates/remote_control/src/models.rs#L5015) `touch_key_ok`) | `key_invalid` |
  | a name empty, over 48 characters or with a control character | `name_empty`, `name_invalid` |
  | a label over 16 characters, an id over 32 or repeated within the preset | `label_invalid`, `id_invalid`, `id_duplicate` |
  | no controls or more than 24, an empty row or one of more than 12 buttons, a chord of more than 4 keys | `no_controls`, `too_many_controls`, `row_buttons`, `chord_keys` |
  | a size outside 48 to 400 px, a stick speed outside 0.05 to 5, an edge past 4000 px, both edges of one axis | `size_invalid`, `speed_invalid`, `place_invalid`, `place_both_edges` |
  | a macro with no step or more than 32, a step with both or neither of a key and a button, a wait over 5 s, the whole over 30 s | `macro_steps`, `step_key_or_button`, `step_wait`, `macro_too_long` |

  A body over 32 KiB is a 413. A name the org already has is a 409 `name_taken`, on a create and on a rename
  alike, decided by the unique `(tenant_id, name)` index. An org keeps at most 50 presets, and a 51st is a
  409 `too_many_presets`.
- **A member's own layout stays in the browser**, and needs no permission (open decision 6, for P4b). The
  server holds only what the org shares.

## The stage keeps every touch

```mermaid
sequenceDiagram
    participant F as Finger
    participant B as Browser
    participant V as Viewer (the stage)
    participant A as roomlerd
    F->>B: touch down on the picture
    B->>V: pointerdown (pointerType touch)
    F->>B: moves past the slop
    Note over B: touch-action: none, so the browser<br/>does not take the drag over
    V->>A: mouse_button {left, down} where it landed
    V->>A: mouse_move ...
    F->>B: lifts
    B->>V: pointerup
    V->>A: mouse_button {left, up}
    Note over B,V: if the browser cancels anyway<br/>(a system gesture, palm rejection)
    B-->>V: pointercancel
    V-->>A: mouse_button {left, up} at the last point
```

- The stage sets `touch-action: none`
  ([`RemoteControl.vue:1052`](../ui/src/views/remote/RemoteControl.vue#L1052), decided by
  [`touchInput.ts:17`](../ui/src/composables/touchInput.ts#L17) `stageTouchAction`). The browser no longer
  pans or zooms the page under a finger on the picture, and the page around the stage still scrolls as usual.
- **A cancelled pointer still lets go.** A cancelled drag is released at its last point; a press the gestures
  had not made yet is dropped. A mouse or pen pointer the browser cancels releases exactly the button it
  pressed ([`touchInput.ts:36`](../ui/src/composables/touchInput.ts#L36) `createPointerPressTracker`,
  [`useRemoteControl.ts:10316`](../ui/src/composables/useRemoteControl.ts#L10316) `onPointerCancel`). In
  relative mode (FR-109) the release is `mouse_release`, which lets go wherever the host's pointer is
  ([`touchInput.ts:57`](../ui/src/composables/touchInput.ts#L57) `releaseForCancelledPointer`).

## Switching it off

| Where | How |
|---|---|
| One browser | `localStorage['roomler-rc-touch'] = 'legacy'` gives the stage back to the browser's own pan and zoom, and sends a finger down the mouse's path as before FR-111 ([`touchInput.ts:10`](../ui/src/composables/touchInput.ts#L10)). A cancelled pointer is released either way: that is a fix, not a mode |
| One device, in one browser | The on-screen controls are off until a preset is chosen. The menu's **Off** removes them and remembers that (`'off'`). Under the `legacy` switch the menu is not offered and no overlay shows |

## What it does not do yet

- No layout editor and no macros in the viewer (P4b). The server keeps an org's presets (P4a), but no
  viewer offers them yet; G1's three presets are built in.
- In trackpad mode two fingers moving together still scroll the host, so a zoomed picture is moved around
  in direct mode, or by zooming out and in again.
- No native touch on the host: `InputMsg::Touch` is still dropped by every injector (P5, optional).

## Related

- [FR-111](fr/FR-111-touch-controls.md): the spec, the phases, the field log.
- [`relative-mouse.md`](relative-mouse.md): `mouse_rel` and `mouse_press`, which the trackpad mode and the
  mouse stick use on an agent that advertises them.
- [`remote-control.md` §6](remote-control.md): the input plane this rides on.
