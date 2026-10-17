# Display match — the host's display follows the viewer, never on a screen someone is using

> **Status** (FR-108, [#2322](https://github.com/gjovanov/roomler-ai/issues/2322); spec:
> [`docs/fr/FR-108-display-follows-the-viewer.md`](fr/FR-108-display-follows-the-viewer.md))
>
> | Phase | What | State |
> |---|---|---|
> | rc.191 | the viewer asks; the agent switches the primary display to the largest mode inside the viewer's stage, and restores it when the session ends | shipped |
> | P1 | the device decides: its owner's `display_match`, and under `auto` whether someone is using the screen; every request answered | shipped (0.4.140); field-verified on Windows |
> | P1b | a locked host switches once unlocked with nobody at it; a person coming back gets their mode back | shipped (0.4.140); field-verified on Windows |
> | P2–P4 | Linux virtual desktops, modes no monitor offers, several displays | not built |

## Why it exists, and what went wrong with it

Remote text is only crisp when the pixels run 1:1 end to end: the host renders at the size the viewer
shows it, the stream carries those pixels untouched, and the viewer paints them unscaled. RDP gets there by
changing the session's resolution. Display match is Roomler's equivalent: the viewer sends its stage size,
and the agent switches the host's primary display to the largest mode that fits inside it
([`display_match.rs:44`](../agents/roomlerd/src/display_match.rs#L44) `pick_display_mode`). The change is
temporary (`CDS_FULLSCREEN`), so even a crash gives the mode back.

The toggle has been **on by default** since 2026-08-02, and until FR-108 nothing asked who was sitting at the
screen. On 2026-10-16 a vmtest guest with its person at the console was switched from 1280×800 to
**640×480** for a whole session: a 1280×720 browser reports a 946×598 stage, and 800×600 is 2 px too tall
(FR-108 §8 row 2). On a machine someone is using, that takes their screen.

## The decision

```mermaid
sequenceDiagram
    participant V as Viewer
    participant A as Agent (worker)
    participant P as presence
    V->>A: rc:display-match {width, height} (on connect, and on toggle)
    A->>A: the owner's display_match
    alt never
        A-->>V: rc:display-match.state {on:false, refused:"disabled_on_device"}
    else always
        A->>A: switch to the largest mode inside the stage
        A-->>V: {on:true, mode:{w,h}}  or  {on:false, refused:"no_fitting_mode" | "switch_failed"}
    else auto
        A->>P: who is at the screen?
        alt locked
            A-->>V: {on:false, refused:"locked"}
        else a person within 5 minutes
            A-->>V: {on:false, refused:"attended"}
        else nobody for 5 minutes
            A->>A: switch
            A-->>V: {on:true, mode:{w,h}}
        end
    end
    Note over V,A: the session's end, or {enable:false}, restores the mode (only the session that applied it)
```

[`display_match.rs:380`](../agents/roomlerd/src/display_match.rs#L380) `may_switch` is the decision, pure and
unit-tested. [`display_match.rs:405`](../agents/roomlerd/src/display_match.rs#L405) `request_for` runs it,
switches, and logs rc.191's lines ("switched host display mode", "could not switch mode") as before. The
control-channel arm sends the answer ([`peer.rs:9464`](../agents/roomlerd/src/peer.rs#L9464)).

### The answers

Every request gets exactly one `rc:display-match.state`
([`display_match.rs:522`](../agents/roomlerd/src/display_match.rs#L522) `state_json`):

| `refused` | It means | The viewer says |
|---|---|---|
| — (`on: true`, `mode: {w, h}`) | the host runs at that mode for this session | "the host runs at W×H for this session" |
| `attended` | someone touched the keyboard or mouse in the last 5 minutes | "someone is using this computer" |
| `locked` | the secure desktop is up (locked, or the logon screen) | "the computer is locked; it switches once unlocked" (P1b) |
| `disabled_on_device` | the owner set `display_match = never` | "this device's owner turned display match off" |
| `no_fitting_mode` | no mode the display offers fits inside the stage | "no display mode fits this window" |
| `switch_failed` | `ChangeDisplaySettingsExW` refused | "the display could not be switched" |
| `unsupported` | not a Windows host | "this device can't switch its display" |

⚠️ An agent from before FR-108 answers nothing, and the viewer then shows the toggle as it always did. A word
this viewer does not know, from a newer agent, is shown as it came and never read as success
([`displayMatch.ts:39`](../ui/src/composables/displayMatch.ts#L39) `describeDisplayMatchRefusal`). The viewer
puts the answer under the toggle ([`RemoteControl.vue:3707`](../ui/src/views/remote/RemoteControl.vue#L3707)
`displayMatchSaid`).

⚠️ `attended` and `disabled_on_device` hand the screen back: a mode this session applied earlier is restored
([`display_match.rs:398`](../agents/roomlerd/src/display_match.rs#L398) `hands_the_screen_back`). `locked`
does not, because the worker cannot touch the secure desktop (below).

## The owner's key

```sh
roomler config set display_match auto     # auto (default) | always | never
```

| Value | Display match switches |
|---|---|
| `auto` | only while nobody is using the screen |
| `always` | whenever a controller asks (rc.191's behaviour), for a machine its owner never sits at |
| `never` | never |

- **Device-owned:** set over the LocalAPI only by the person at the console
  ([`lib.rs:2838`](../crates/localapi/src/lib.rs#L2838)), and absent from `DesiredConfig`, so a server cannot
  push it ([`models.rs:7554`](../crates/remote_control/src/models.rs#L7554)). A server that could set it could
  take every screen it reaches.
- **Read live** at every request ([`display_match.rs:310`](../agents/roomlerd/src/display_match.rs#L310)
  `set_device_policy`).
- ⚠️ A value this build does not know reads as `never`, the strictest, and never fails the config file
  ([`config.rs:2379`](../crates/agent-core/src/config.rs#L2379) `DisplayMatchPolicy`).

## Who is at the screen

"Nobody" means what FR-99's `lock_on_end = auto` means by it: no local input for **5 minutes**
(`lock_on_end::UNATTENDED_AFTER`). It is read FR-92's way, with no hook, no raw input and no key polling:
the session's idle clock, `GetLastInputInfo`.

That clock moves for every input, and FR-108's P0 measured both kinds moving it: the person's
(QMP on a vmtest guest) and the controller's injected moves. So
[`presence.rs:96`](../agents/roomlerd/src/presence.rs#L96) `observe` follows it and keeps the last move
**the machine does not explain**:

| Input | Noted by | On the clock as |
|---|---|---|
| the controller's injected events | the input arbiter ([`keep_busy/mod.rs:89`](../agents/roomlerd/src/keep_busy/mod.rs#L89) `note_remote_input`) | the machine's |
| keep busy's own pointer moves | keep busy's mover ([`keep_busy/mod.rs:100`](../agents/roomlerd/src/keep_busy/mod.rs#L100) `note_own_move`) | the machine's |
| anything else | — | **a person's** |

A move within 250 ms of a machine input is that input
([`presence.rs:34`](../agents/roomlerd/src/presence.rs#L34) `MACHINE_SLACK_MS`). A sampler reads the clock
every 500 ms from the worker's start ([`presence.rs:186`](../agents/roomlerd/src/presence.rs#L186) `start`),
so a request mid-session still tells the controller's input from a person's.

- **Before the first reading nothing is known**, and an unknown is never read as an empty chair: the first
  reading counts as a person's.
- **The decision** ([`presence.rs:57`](../agents/roomlerd/src/presence.rs#L57) `decide`): locked wins over
  everything; a lock after the person's last input means away (P1b, below); otherwise a person's input
  inside the window is `attended`.
- **The source scan** ([`presence.rs:412`](../agents/roomlerd/src/presence.rs#L412)) fails the build on a
  hook, raw input or a keyboard poll in `presence.rs` or `display_match.rs`, FR-92's rule.

### Locked waits for the unlock (P1b)

P0 tried it: on the lock screen the worker's `ChangeDisplaySettingsExW` returns `-1`, and the screen stays as
it was. So a locked host answers `locked`, and the request waits
([`display_match.rs:405`](../agents/roomlerd/src/display_match.rs#L405) `request_for` keeps it per session).
An unattended office PC is usually locked, so this is the main path, not an edge.

At the session's unlock the device decides again
([`display_match.rs:474`](../agents/roomlerd/src/display_match.rs#L474) `on_unlock`, called from the
session's lock watcher, [`peer.rs:1314`](../agents/roomlerd/src/peer.rs#L1314)):

| Who unlocked it | What the clock shows | The answer |
|---|---|---|
| the controller, typing the password through the session | machine input only since the lock | **switched** (`on: true`): the person left by locking |
| the person, at the keyboard | a person's input after the lock | `attended` |

⚠️ The rule that makes the first row work: **a lock after the person's last input means they are away**,
however recent that input was ([`presence.rs:128`](../agents/roomlerd/src/presence.rs#L128)
`away_since_lock`). Without it, someone who locked their screen and walked off a minute ago would still
read as `attended` for four more minutes, and the request would be refused for nothing.

## The person comes back (P1b)

While a session holds a mode it applied, a watcher checks four times a second
([`peer.rs:1337`](../agents/roomlerd/src/peer.rs#L1337), Windows only). It hands the screen back at once
and tells the viewer ([`display_match.rs:507`](../agents/roomlerd/src/display_match.rs#L507)
`on_tick`):

| What changed | The answer | Restored |
|---|---|---|
| a person's input (under `auto`) | `refused: attended` | yes |
| the owner set `display_match = never` | `refused: disabled_on_device` | yes |
| the screen locked | — | no: nobody came back, and the secure desktop cannot be switched anyway |

The mode is not re-applied until the viewer asks again (toggle off and on): a returning person never fights a
timer for their own screen ([`display_match.rs:488`](../agents/roomlerd/src/display_match.rs#L488)
`hand_back`).

## What it does not do

- macOS and Wayland hosts: display match is Windows only (Linux virtual desktops come in P2).
- DPI and scaling are never changed.
- One primary display; several displays at once are P4.

## Related

- [FR-108](fr/FR-108-display-follows-the-viewer.md): the spec, the field log, the phases.
- [FR-92](fr/FR-92-keep-busy.md): the idle clock and the no-hooks rule.
- [Privacy mode](privacy-mode.md): the other thing a session does to the screen of the person at the host.
- [`remote-control.md` §18.6.1](remote-control.md): the zoom pill that shows whether the pixels run 1:1.
