# FR-45 — Portal capture: Wayland where there is no scanout

**Issue:** [#1041](https://github.com/gjovanov/roomler-ai/issues/1041) · **Status:** P1 → P3c COMPLETE and field-verified — **a Wayland desktop is captured through the portal and delivered to the daemon as a correct picture**. P4 (RemoteDesktop input) BUILT — field verification pending · **Owner:** agent / capture

## Goal

Capture a **Wayland desktop that has no DRM scanout** — WSL2, containers,
nested compositors, cloud VMs with no display controller — via
`xdg-desktop-portal`'s ScreenCast interface and PipeWire.

[FR-36](FR-36-wayland-capture.md) closed by capturing **below** the compositor
with DRM/KMS. That works and is the right answer for unattended access, but it
requires a real CRTC with live scanout. Where there is none, there is nothing
to read, and no amount of configuration creates one.

## Why now — the two halves are measured and they do not meet

| | Wayland desktop | DRM scanout | HW encode |
|---|---|---|---|
| `scw-m2-asahi` (Apple Silicon) | ✅ | ✅ `apple-drm`, 4096×2160 | ❌ **none** — Asahi has no video-encode driver (no `/dev/video*`, no `/dev/media*`, no AVD/encode module) |
| WSL2 on an RTX-class laptop | ✅ (nested GNOME under WSLg) | ❌ **`/dev/dri` does not exist** | ✅ `av1_nvenc` / `hevc_nvenc` / `h264_nvenc`, measured `avg_encode_ms=10.4` |

So today the only host that can *capture* Wayland cannot *encode* in hardware,
and the only host that can encode in hardware cannot be captured. **The portal
is the one path that joins them**, and it is the path FR-36 deliberately
demoted to a fallback and never built.

Measured on WSL2 2026-08-31 through the Xvfb virtual desktop, to show the
encode half already works there:

```
AV1 4:2:0 HW (av1_nvenc) · dec HW · 1920×1080 · 77 kbps · ~41 ms
avg_encode_ms=10.4     (Asahi software openh264: 15–21)
```

## ⚠️⚠️ This is the ATTENDED path. It must never be sold as unattended.

FR-36's P0 established this and it has not changed:

- ScreenCast needs an **interactive picker** and an **active user session**.
- `restore_token` only avoids re-prompting *after a human approved once*.
- **While the session is LOCKED, mutter refuses to create *or restore* a
  screencast**, and the failed attempt tends to **consume the saved token**.
- Token behaviour is **inconsistent across compositors**; reboot/logout
  invalidates it.

So this FR does **not** replace FR-36 and does not deliver greeter or
locked-screen capture. It serves the case FR-36 cannot reach: a logged-in user
on a machine with no scanout. The backend priority stays
**DRM/KMS → portal → X11**, with the portal picked only when DRM finds no CRTC.

## Design

| Phase | What | Kill switch |
|---|---|---|
| **P1** ✅ | Detect whether `org.freedesktop.portal.ScreenCast` is actually exposed, and report WHY not, in `capture-smoke` (`capture/portal.rs`). Uses zbus, which is pure Rust and adds no system `.so`. | n/a (read-only) |
| **P2a** ✅ | `portal-helper` hidden subcommand, spawned as the console user with that session's bus. Proven by running `detect()` from the daemon and getting `available` where it previously could only say `no-session-bus`. ⚠️ Built with the verified **privilege drop**, not `systemd-run` as planned — see below. | `ROOMLERD_PORTAL_CAPTURE=0` |
| **P2b** ✅ | The helper opens the ScreenCast session: `CreateSession` → `SelectSources` → `Start` → `OpenPipeWireRemote`, giving a PipeWire node id. Consent handled via `persist_mode`/`restore_token`. ⚠️ The fd **stays in the helper**, and so does the token — see the corrected decisions below. Field-verified end to end: root → helper → live session, **15 ms, no dialog**. | `ROOMLERD_PORTAL_CAPTURE=0` |
| **P3a** ✅ | Reach `libpipewire` by **`dlopen`**, never a link, and connect the portal's fd to it. Verified three ways: **0** `DT_NEEDED` entries on x86_64 AND aarch64; a live connect (`libpipewire 1.4.11`); and graceful degradation with the library hidden in a mount namespace. Zero new dependencies. | same |
| **P3b-i** ✅ | SPA POD serialisation in Rust. Forced, not chosen: `nm -D --defined-only libpipewire-0.3.so.0 \| grep -c '^spa_'` is **0**, because the whole builder API is `static inline` — there is nothing to `dlsym`, and linking would undo P3a. Byte-exact tests against the header layout, and **validated by PipeWire in P3b-ii** — the daemon parsed it and answered with a fixated format. | same |
| **P3b-ii** ✅ | `pw_stream_new`/`add_listener`/`connect` with the EnumFormat param, and the parser `param_changed` needs. **This validates P3b-i** — PipeWire parsed the hand-written POD. Field-verified as root: **negotiated BGRx 1920x1080 (libpipewire 1.4.11) in 12 ms**, and BGRx was our first preference. | same |
| **P3c-i** ✅ | `process` → `pw_stream_dequeue_buffer` → inspect → queue back. **A picture exists.** Field-verified as root: 3 frames, **MemFd**, stride 7680 (= 1920x4), 8 294 400 bytes (= a full frame), **8291/8320 sampled bytes non-zero**, and the checksum CHANGES between runs — live pixels, not a constant. | same |
| **P3c-ii** ✅ | Frames to the daemon and the **sixth `ScreenCapture` backend**, picked after DRM and before X11. ⚠️ A pipe and a copy, **not** `SCM_RIGHTS` — see the corrected decision below. Field-verified through `capture-smoke` as root: `backend=portal`, **delivered=5 empty=0**, 1920x1080 Bgra, `mean_ms=27.70`, and the dumped frame is a correct picture. | `ROOMLERD_PORTAL_CAPTURE=1` |
| **P4** 🔨 BUILT (2026-09-01, unverified in the field) | Input via the portal's **RemoteDesktop** interface, riding the SAME session as capture — `CreateSession`/`Start` move to RemoteDesktop, `SelectDevices` (keyboard+pointer, persist on v2+) slots in before the unchanged `SelectSources`, so ONE consent dialog covers see+touch and one restore token covers both (stored apart from the capture-only token: `portal-restore-token-rd`). The daemon's input arbiter forwards `InputMsg` JSON lines to the helper's stdin; the helper maps them to `Notify*` (evdev keycodes via the shared FR-36 table; typed text as Unicode keysyms, layout-proof; absolute motion in the stream's LOGICAL size). Falls back to capture-only where the portal has no RemoteDesktop (wlr, measured). Motivation measured 2026-08-31: uinput works in WSL2 and libinput even enumerates the device — but a NESTED compositor reads its parent, not evdev, so nothing consumes the events. | `ROOMLERD_PORTAL_INPUT=0` (config `portal_input`) |

### The seam is unchanged

`ScreenCapture` (`capture/mod.rs`), chosen in `open_default`. DRM is the fifth
backend; this is the sixth. No pump changes.

## ⚠️ The dependency question is the biggest design risk, and it is not theoretical

FR-36 P4b rejected `libxkbcommon` on exactly these grounds and the same
reasoning applies harder here: **linking `libpipewire` would put it in
`roomlerd`'s `DT_NEEDED` on every Linux build.** Headless fleet hosts —
cluster nodes, containers, the ones that will never run a portal — have no
reason to carry it, and a missing `.so` does not degrade a feature: **the
loader refuses to start the daemon at all.** This project has already paid for
that once, when vendored FFmpeg dylibs baked a Homebrew path into the macOS
agent and dyld killed it at launch on every end-user Mac.

Three options, in the order they should be evaluated:

1. **`dlopen` at runtime** behind the existing feature gate — the daemon starts
   everywhere and the backend simply reports unavailable where the library is
   absent. Most code, least deployment risk.
2. **A separate helper binary** that links PipeWire and streams frames over a
   socket, spawned via `current_exe()` — the `caps-probe` precedent
   (`main.rs`, `#[command(hide = true)]`), so no new packaged artifact. The
   daemon's own graph stays clean.
3. Link it directly and accept a hard runtime dependency. **Only if** the
   `.deb`/`.pkg` can be made to carry it, and even then it costs every headless
   host disk and risk for nothing.

⚠️ Whichever is chosen, **prove it by starting the daemon on a host without
PipeWire installed**, not by reasoning about it.


## ⚠️⚠️ P1 found the thing that shapes P2, and it is not PipeWire

**The daemon runs as root and has no D-Bus session bus.** Detection from the
daemon's own context returns `no-session-bus` on a host whose GNOME Wayland
session is right there and active. That is not a test artefact — the portal is
**per user session** by construction, and a root daemon is not in one.

So **P2/P3 need a session-resident component before they need a single line of
PipeWire code**: something running as the logged-in user that opens the portal
session and hands the result back to the daemon. That partially resurrects the
"session broker" FR-36 superseded — legitimately this time, because FR-45 *is*
the attended path, and the reason FR-36 rejected it (it made the portal
reachable but the portal refuses when locked) does not apply to a case that
never claimed to work locked.

⚠️ **And a second, harder blocker.** On `scw-m2-asahi`, running **inside** the
active GNOME Wayland session with `xdg-desktop-portal-gnome` installed, the
portal exposes **neither `ScreenCast` nor `RemoteDesktop`** — verified
independently with `busctl`, which lists Account, Camera, FileChooser,
Notification and eleven others but not those two. FR-36 measured the same thing
and attributed it to an X11 session; it reproduces under Wayland, so that
explanation was wrong. **P2 cannot start until this is understood**, because
there is currently no host in the fleet that offers the interface P2 would
call.


## ✅ The P2 blocker is diagnosed and cleared (2026-08-31)

`scw-m2-asahi` exposed no ScreenCast for a reason that had nothing to do with
the session type FR-36 blamed, and nothing to do with a missing package:

1. `XDG_CURRENT_DESKTOP=GNOME`, `XDG_SESSION_TYPE=wayland`, `gnome.portal`
   installed, and **mutter was already exposing its own
   `org.gnome.Mutter.ScreenCast` / `RemoteDesktop` APIs** — the compositor side
   was never the problem.
2. **`xdg-desktop-portal-gnome` was `inactive (dead)`.** It is a `static`,
   D-Bus-activated unit and nothing in that session had triggered it, so the
   frontend served only the interfaces it can provide alone (Account, Camera,
   FileChooser, Notification …).
3. ⚠️ **Starting the backend was not enough.** The frontend caches its backend
   selection at startup; the interfaces appeared only after
   `systemctl --user restart xdg-desktop-portal` *with the backend already up*.

```
portal=available (screencast v5, remote_desktop=true)
```

⇒ **P2 is unblocked**, `RemoteDesktop` is present so **P4 is viable**, and the
`Available` branch of the P1 detector — the one branch that had never been
exercised — is verified.

⚠️ **The ordering fragility is itself a design input.** A host can have every
package installed and still offer nothing, depending on service start order, so
the agent must **detect at session time and never cache** the answer. The P1
advice string was corrected accordingly: it used to say "install the backend",
which on this host would have sent the reader to a package that was already
there.


## P2 plan — the session-resident component already exists as a pattern

P1 found that the daemon is root and has no session bus. The obvious reading is
"FR-45 needs a broker written". It does not: `agents/roomlerd/src/companion.rs`
already spawns a process **as the console user with the session bus wired up**,
and has done since FR-27 used it as a consent-prompt surface.

The Linux arm (`#[cfg(all(unix, not(target_os = "macos")))]`) does exactly what
P2 needs:

```rust
Command::new("systemd-run")
    --uid={sess.uid}
    --setenv=XDG_RUNTIME_DIR=/run/user/{uid}
    --setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus
```

That is the same environment that had to be hand-built as
`runuser -u m1 -- env XDG_RUNTIME_DIR=… DBUS_SESSION_BUS_ADDRESS=…` to make P1
detection succeed at all. `graphical_session()` already resolves the session
and uid; there is nothing to invent.

### 🔑 And it answers the dependency question — but only in one combination

| shape | session context | `DT_NEEDED` on the daemon |
|---|---|---|
| link PipeWire into the daemon | ❌ no session bus | ⛔ **stops headless daemons starting** |
| hidden subcommand of the same binary, PipeWire **linked** | ✅ | ⛔ **same binary, same `DT_NEEDED`** |
| separate helper binary | ✅ | ✅ but a new packaged artifact + version skew |
| **hidden subcommand + `dlopen`** | ✅ | ✅ |

⚠️ The middle row is the trap worth naming: a helper *subcommand* does **not**
by itself solve the linkage problem, because it is the same ELF. Only `dlopen`
(or a genuinely separate binary) does. **`roomlerd portal-helper` +
`dlopen(libpipewire)`** takes the session context from the companion pattern and
the deployment safety from `dlopen`, with no new artifact — the `caps-probe`
precedent (`current_exe()`, `#[command(hide = true)]`).

### Phases as they now stand

- **P2a** ✅ **shipped and field-verified** (#1054) — `portal-helper` hidden
  subcommand; the daemon spawns itself as the console user pointed at that
  session's bus, the child reports one `ROOMLER_PORTAL_JSON:{…}` line, and
  `detect_in_session()` parses it back.

  ⚠️ **Departed from the plan above, deliberately.** #1050 said `systemd-run
  --uid`, mirroring the consent companion. It is spawned as a **direct child
  with the verified privilege drop** instead, for two reasons P2b needs: a
  direct child can be handed the PipeWire fd over `SCM_RIGHTS` on a
  socketpair, and it dies with the daemon that owns it. It also drops
  privilege without depending on systemd being the init system. Not
  `CommandExt::uid()` — that leaves the child in root's supplementary groups,
  a silent retention bug rather than a visible failure.

  🔑 **The near-miss this phase existed to avoid is still live for P3**: a
  helper *subcommand* does not by itself solve the dependency problem, because
  it is the same ELF and so the same `DT_NEEDED`. The subcommand bought the
  session context; only `dlopen` buys the linkage.
- **P2b** ✅ **shipped and field-verified end to end** (#1059) —
  `CreateSession` → `SelectSources` → `Start` → `OpenPipeWireRemote`.

  ⚠️⚠️ **This plan contradicted itself and the contradiction is now resolved.**
  It said P2b passes the PipeWire fd to the daemon over `SCM_RIGHTS` (inherited
  from FR-36's design) *and* that P3 consumes PipeWire inside the helper. Those
  cannot both be true. Decided:

  > **The helper consumes PipeWire. The fd never crosses to the daemon.**

  1. **Fault isolation** — the `encode::caps` lesson exactly: third-party
     driver code that faults inside the daemon costs a crash-loop, not a
     degraded feature. `libpipewire` loads SPA plugins on vendor GPU stacks.
  2. **It is not slower** — PipeWire negotiates a *pool* of buffers, so those
     fds cross once and per frame only "buffer N is ready" flows. Frames
     crossing a process boundary is not pixels being copied.
  3. **The helper already runs as the session's user**, whose PipeWire it is.

  `SCM_RIGHTS` therefore moves to P3, carrying **buffer** fds.
- **P3a** ✅ **shipped and field-verified** (#1062) — `dlopen` reaches
  `libpipewire`, and the portal's fd connects to it. The daemon never links it,
  and after the P2b decision never connects to it either.

  🔑 **Proven by construction, not by finding a bare host.** `readelf -d` shows
  **zero** `DT_NEEDED` entries matching `pipewire`/`libspa` on both
  architectures — if it is not a load-time dependency, the loader never looks
  for it, which is stronger than testing one host that happens to lack it.
  Graceful degradation was then shown *live* by bind-mounting the library away
  inside a mount namespace: the binary ran, the portal handshake still
  succeeded, and PipeWire reported unavailable naming the sonames it tried.

  ⚠️ It does **not** prove frames flow. That is P3b, and a non-null pointer is
  not a picture.
- **P3b** — format negotiation (SPA PODs) and buffer delivery, then the sixth
  `ScreenCapture` backend.
- **P4** — RemoteDesktop input in the same helper and the same session. BUILT
  2026-09-01; the design decisions worth knowing before editing:
  - **One session, both halves.** `SessionKind::WithInput` re-homes
    `CreateSession`/`Start` onto RemoteDesktop and adds `SelectDevices`;
    `SelectSources`/`OpenPipeWireRemote` stay on ScreenCast against the shared
    session path. One dialog, one token, one PipeWire node.
  - **The input wire is `InputMsg` JSON lines on the helper's stdin.** Same
    binary both ends, serde already on the type, so no second wire enum and no
    version-skew surface; an unparseable line costs that event only.
  - **Routing is per-event, not per-backend-choice**
    (`portal::input_route::try_route` in the arbiter's single inject funnel):
    the OS injector is created lazily at the FIRST event while the portal
    capture opens concurrently, so a one-time choice would race startup.
    Registration is generation-checked so a stale capture `Drop` cannot tear
    down its successor's route. Overload DROPS rather than falling through —
    splitting one gesture across two injection backends is worse than a lost
    event.
  - **Keys are evdev keycodes** (`NotifyKeyboardKeycode`, the shared
    `input::hid_evdev` table FR-36 built — moved out of the uinput backend
    because the two consumers sit behind different features). **Typed text is
    Unicode keysyms** (`NotifyKeyboardKeysym`, codepoint+0x01000000 above
    Latin-1) — layout-proof, the exact property the uinput backend cannot
    offer.
  - **Absolute motion is addressed in the stream's LOGICAL size** (the
    portal's advertised stream size), not the negotiated pixel size — they
    differ under a HiDPI scale factor. Wire coords are normalised 0..1, so
    the conversion is one multiply in the helper.
  - ⚠️ **Axis sign follows libinput (positive = down/right), NOT evdev** —
    copying the uinput backend's REL_WHEEL inversion here scrolls backwards.
    Pinned by a unit test; still needs a field check.
  - ⚠️ `ROOMLERD_PORTAL_INPUT` defaults **ON** (unlike the capture flag): it
    is inert unless a portal capture is already live, and on those hosts the
    portal is the only input path with a reader behind it — a capture-only
    default would ship a read-only session and call it working.

## Acceptance criteria

- [ ] ⛔ **BLOCKED, and now understood** (2026-09-01): WSL2 has no `/dev/dri`, so
      wlroots has no renderer and `wlr-screencopy` can advertise no format — the
      portal path is blocked by the *same* missing device as the DRM path. A
      **nested GNOME Wayland session in WSL2** (per the WSLg + `gnome-shell
      --nested` recipe) is captured and rendered in the browser
- [ ] That session encodes with **`*_nvenc`**, and `avg_encode_ms` is within
      ~2× the 10.4 ms already measured on that host
- [x] `roomlerd capture-smoke` reports **whether the portal is available and
      why not**, on a host where it is absent — FR-36 measured a host where
      `xdg-desktop-portal` was running yet exposed **neither ScreenCast nor
      RemoteDesktop**, so availability must be *detected*, never assumed
- [x] **The daemon still starts on a host with no PipeWire library present.**
      Verified by running it, not by inspection — `dlopen` only, **0**
      `DT_NEEDED` entries on both architectures, and a live run with the
      library bind-mounted away that still completed the portal handshake
- [x] Backend order holds: a host with a real CRTC still picks **DRM**, and the
      portal is chosen only when DRM finds none — measured on a host that has
      BOTH: DRM-only → `backend=drm`; **both flags → still `backend=drm`**;
      neither → the portal does not engage at all (no helper, no dialog)
- [x] X11/Windows/macOS unchanged; the kill switch restores the current
      cascade — with no flag set the cascade is byte-for-byte what it was, and
      the portal code is Linux-only, feature-gated AND env-gated, so it cannot
      reach the other platforms. Windows/macOS CI lanes green throughout
- [x] Field-verified with the **before** state recorded beside the after — done for P2a (`no-session-bus` → `available`, with an in-session control); each later phase repeats it
- [ ] The spec and the UI both say **attended-only** — no greeter, no locked
      screen
- [ ] **P4: input lands.** On a portal-captured GNOME Wayland session, pointer
      motion/click and typed text injected from the controller visibly act on
      the host (cursor moves in the captured frames — cursor_mode=embedded
      makes this self-evidencing), scroll direction is CORRECT (the sign
      convention is a unit test, not yet a field fact), and
      `ROOMLERD_PORTAL_INPUT=0` restores a view-only session
- [ ] **P4: one dialog, then none.** The first input session shows ONE consent
      dialog covering screen+devices; the next session restores from
      `portal-restore-token-rd` without prompting (RemoteDesktop v2+)

## Open decisions

- ✅ ~~**Which of the three dependency shapes.**~~ **Decided: hidden subcommand +
  `dlopen`.** A helper subcommand alone does NOT solve the linkage problem — it
  is the same ELF, so the same `DT_NEEDED`. Only `dlopen` (or a separate binary,
  which costs an artifact and version skew) does. See the P2 plan table.
- ✅ ~~**Does `/dev/uinput` work in WSL2?**~~ **ANSWERED 2026-08-31 — P4 is
  MANDATORY, and the reason is not the one expected.**
  - uinput itself works: `CONFIG_INPUT_UINPUT=m`, module loads, FR-36's
    injector creates a device (`has_permission=true`).
  - The evdev subsystem works too — a *persistent* uinput device appears as
    `/dev/input/event0` and **libinput enumerates it on `seat0`**. `/dev/input/`
    is normally empty only because nothing has created a device, not because
    the subsystem is missing. (⚠️ FR-36's injector destroys its device on drop,
    so a quick `ls` afterwards shows nothing and reads as failure.)
  - **But a NESTED compositor does not read evdev.** Weston under WSLg loads
    `wayland-backend.so` and uses `xdg_wm_base` — no libinput, no udev, no
    device enumeration. A nested compositor takes input from its **parent**,
    and WSL2 can only run nested or headless compositors because there is no
    DRM to run a libinput-backed seat on.
  ⇒ Injected events are published and nothing consumes them. **The portal's
  RemoteDesktop interface is the only input path for this case**, which makes
  ScreenCast + RemoteDesktop a coherent pair rather than capture-only.
  2026-08-31: `CONFIG_INPUT_UINPUT=m`, the module loads, and FR-36's injector
  created a device and accepted a pointer move (`input: backend=uinput`,
  `has_permission=true`). So injection is not the obstacle.
  ⚠️⚠️ **`/dev/input/` is otherwise EMPTY on WSL2** — no evdev devices at all,
  because WSLg's input arrives over RDP rather than from libinput. A uinput
  device therefore publishes events that, on present evidence, **nothing in
  WSL2 consumes**. So P4 probably survives — not because uinput fails, but
  because the reader is missing.
  **Still unproven, and it is the thing to test first:** whether a *nested*
  compositor (`gnome-shell --nested` under WSLg) reads evdev. If it does, P4
  disappears; if it does not, the portal's RemoteDesktop interface is the only
  input path and P4 is mandatory. One nested session answers it.
- **Restore tokens: store or not.** They are per-compositor, invalidated by
  reboot/logout, and a failed locked-session attempt can consume one. Storing a
  token that is silently dead is worse than prompting.
- Multi-monitor selection: the portal's picker returns whichever outputs the
  user chose, not a stable index — the mapping to `DisplayInfo` needs thought.
- Whether the portal path should be offered on hosts that *do* have DRM (e.g.
  as an operator override when DRM capture misbehaves).

## Out of scope

- Replacing FR-36. DRM stays the unattended path and the default where a CRTC
  exists.
- Greeter and locked-screen capture. The portal structurally refuses both.
- macOS/Windows. This is a Linux desktop-portal concern.

## Field-verification log

| date | build | result |
|---|---|---|
| 2026-08-31 | 0.4.33, WSL2 | **`/dev/dri` absent** — DRM capture impossible. `/dev/dxg` + `libnvidia-encode.so` present; caps probe reports `ffmpeg-av1_nvenc`, `ffmpeg-hevc_nvenc`, `ffmpeg-h264_nvenc` on an RTX 5090 |
| 2026-08-31 | 0.4.33, WSL2 | Xvfb virtual desktop + `av1_nvenc`: 1920×1080, `avg_encode_ms=10.4`, 77 kbps idle, ~41 ms. Establishes the encode half works; only capture is missing |
| 2026-08-31 | 0.4.33, Asahi | Wayland captured via DRM at 1920×1080, but **software encode only** — no video-encode driver exists for Apple Silicon on Linux |
| 2026-08-31 | 0.4.33, WSL2 | **uinput measured, and it reframes P4.** `CONFIG_INPUT_UINPUT=m`, module loads, FR-36's injector created a device and accepted a move (`has_permission=true`) — injection is not the obstacle. But `/dev/input/` is **empty** (WSLg takes input over RDP, not evdev), so nothing appears to consume those events. P4 likely survives because the *reader* is missing, not the writer. Untested: whether a nested `gnome-shell` reads evdev |
| 2026-08-31 | 0.4.33 + weston 13, WSL2 | **The nested-compositor input question, ANSWERED.** A persistent uinput device appears as `/dev/input/event0` and **libinput enumerates it on `seat0`** — so the evdev subsystem is fully alive in WSL2. But weston under WSLg loads `wayland-backend.so` / `xdg_wm_base` with **no libinput, no udev, no device enumeration**: a nested compositor reads its PARENT, not evdev. And WSL2 can only run nested or headless compositors, having no DRM to host a libinput seat. ⇒ **P4 is mandatory**; the portal's RemoteDesktop is the only input path here |
| 2026-08-31 | P1, WSL2 | `portal=no-screencast — xdg-desktop-portal is running but exposes no ScreenCast — install the backend matching your compositor`. The FR-36 case, now named rather than guessed at |
| 2026-08-31 | P1, Asahi **as root** | `portal=no-session-bus`. ⚠️ **This is the architecture, not a test artefact** — the daemon is root, the portal is per-user-session, so P2/P3 need a session-resident component before any PipeWire code |
| 2026-08-31 | P1, Asahi **inside the GNOME Wayland session** | `portal=no-screencast`, **independently confirmed with `busctl`** (lists Account, Camera, FileChooser, Notification … but neither ScreenCast nor RemoteDesktop) despite `xdg-desktop-portal-gnome` being installed. ⛔ **No fleet host currently exposes the interface P2 would call.** FR-36 blamed an X11 session for this; it reproduces under Wayland, so that explanation was wrong |
| 2026-08-31 | **P2 blocker diagnosed and cleared** | Not the session type (FR-36's guess) and not a missing package. `xdg-desktop-portal-gnome` was `inactive (dead)` — a `static` D-Bus-activated unit nothing had triggered — while mutter was already exposing its own ScreenCast/RemoteDesktop APIs. ⚠️ Starting the backend alone did NOT help: the frontend caches its backend selection at startup and had to be restarted after it. Result: `portal=available (screencast v5, remote_desktop=true)` — P2 unblocked, P4 viable, and the detector's `Available` branch exercised for the first time |
| 2026-08-31 | **P2a, Asahi — the loginctl call underneath it never worked** | `loginctl list-sessions --no-legend --no-pager -o value -p Id` — what `graphical_session()` ran — **exits 1 with empty stdout**, measured on systemd **255** (Ubuntu 24.04, two hosts) and **257** (Fedora 42): those are `show-*` options and `list-sessions` rejects them. The function read that as an empty session list, i.e. “nobody is at this machine's screen”, on every Linux host, always. ⚠️ The column layout also differs between the two releases (257 inserts `LEADER` and `CLASS`), so only column one is safe to parse; both real samples are now in a test |
| 2026-08-31 | **P2a FIELD-VERIFIED — 0.4.37, Asahi, GNOME Wayland** | Three runs, one binary. **CONTROL** as `m1` inside the session: `available (screencast v5, remote_desktop=true)`. **BEFORE** as root, plain `detect()` (the P1 behaviour): `no-session-bus`. **AFTER** as root through `detect_in_session()`: `available (screencast v5, remote_desktop=true)` — matching the control exactly. Root's environment was confirmed to carry none of `XDG_RUNTIME_DIR` / `DBUS_SESSION_BUS_ADDRESS` / `DISPLAY` / `WAYLAND_DISPLAY`, i.e. the daemon's own situation. 🔑 The AFTER trace shows **both** halves: the child's own line on inherited stderr, then the parent's parsed verdict |
| 2026-08-31 | P2a — what the loginctl fix could **not** be shown to fix here | The claim that the broken lookup also stopped FR-27's consent companion is **reasoned, not observed on this host**: `roomler-desktop` is not installed on Asahi, so `ensure_running_inner` returns `Unsupported` one step BEFORE reaching `graphical_session()` (the daemon log carries 15 “prompts go to the desktop companion” lines and no session-lookup failure). What IS measured: the old command exits 1 on both systemd versions, and P2a — which calls the same function — works only with the fix |
| 2026-08-31 | **P2b — zbus panics inside a tokio runtime, again** | The first field run of the handshake died on `Cannot start a runtime from within a runtime` before making a single portal call. P1's `detect()` already ran its D-Bus work on its own thread for exactly this reason; `open()` was written without the guard. 🔑 The fix put the thread INSIDE `open()` rather than at the call site, so the next entry point cannot reintroduce it — the same shape as the `ResolvedSessionPolicy` lesson: make the mistake unrepresentable, not merely fixed |
| 2026-08-31 | **P2b field-verified through `Start` — Asahi, GNOME Wayland** | The live bus objects are the evidence: `…/session/1_170/roomler_ss_472905_1` (CreateSession succeeded) and `…/request/1_170/roomler_start_472905_3` (Start pending). The pid in both tokens is the helper's, and `1_170` is its unique name `:1.170` mangled per the portal spec — so **the request-path derivation is confirmed in the field**, which matters because getting it wrong hangs forever rather than failing. `xdg-desktop-portal-gnome` up, `Start` outstanding 7+ min: the consent dialog is waiting to be answered |
| 2026-08-31 | P2b — what could **not** be observed | The dialog's VISIBILITY is inferred from the pending `Request` plus a live backend, not seen: GNOME refuses `org.gnome.Shell.Screenshot` to unsandboxed callers, and `capture-smoke` does not wire FR-36's config-gated DRM backend, so the screen could not be photographed. ⚠️ Also: `pkill -x roomlerd` on a host reached over the overlay kills the daemon carrying your own SSH — ~40 s of no access until systemd restarted it |
| 2026-08-31 | **P2b COMPLETE — 0.4.37, Asahi, GNOME Wayland** | Three passes, one binary. **1.** As the user, no stored token: a consent dialog, answered by a human, **1,831,429 ms**; returns `node_id=83`, `1920x1080`, `pipewire_fd_ok=true`, `cursor_mode_used=2` (embedded, of `available_cursor_modes=7`), `available_source_types=7`. **2.** As the user, token stored: **15 ms, no dialog**, `restore_token_sent=true`, same node. **3.** THE PRODUCTION PATH — as **root** through the session helper: **15 ms**, `node_id=83`, `pipewire_fd_ok=true`. 🔑 The 122,000× gap between pass 1 and pass 2 is what makes “did it prompt?” falsifiable rather than asserted. Token file `600 m1`, 36 bytes |
| 2026-08-31 | **P2b — the dialog was NOT where it was looked for** | `Start` sat pending 30 min while the screen showed no dialog. The backend log had the clue — `xdg-desktop-por[392832]: Failed to associate portal window with parent window` — because `parent_window` is `""` for a CLI caller. ⚠️ Diagnosing this needed the product's OWN capture: GNOME refuses `org.gnome.Shell.Screenshot` AND `org.gnome.Shell.Introspect.GetWindows` to unsandboxed callers, so FR-36's DRM backend was the only way to see the screen — via `ROOMLERD_DRM_CAPTURE=1`, since `capture-smoke` does not register the S2 config fallbacks the daemon does |
| 2026-08-31 | **P2b — the report was carrying the credential** | `SessionReport` documented that the daemon never sees the restore token, and carried it. Noticed because the helper's stdout, redirected to a log for the field test, had the token in plaintext. `open()` now loads and stores it internally and the report says only WHETHER a grant was persisted. 🔑 A caller that cannot hold a credential cannot leak it — stronger than a caller that holds it and is careful |
| 2026-08-31 | **P3a field-verified 3 ways — 0.4.37, Asahi + WSL** | **(a) Structural:** `readelf -d` reports **0** `DT_NEEDED` entries matching `pipewire`/`libspa` on the x86_64 AND aarch64 builds, `ldd` shows nothing, and the binary runs — so the loader can never refuse to start over a library that is not linked. **(b) Success:** `portal-helper --screencast` → `pipewire: connected (libpipewire 1.4.11)` with `node_id=83`, reached entirely through `dlopen`. **(c) Failure:** with `libpipewire-0.3.so.0` bind-mounted over inside a mount namespace, the binary still ran, the portal handshake still succeeded (`node_id=82`, 14 ms) and PipeWire degraded to `unavailable — libpipewire not present (tried …)`. Zero dependencies added |
| 2026-08-31 | P3a — a scare I caused and then disproved | After the namespace test I read `stat -c%s /usr/lib64/libpipewire-0.3.so.0` as **27 bytes** and concluded the bind mount had leaked and broken the host's PipeWire. It had not. ⚠️ **GNU `stat` does not dereference a symlink by default** — that path IS a symlink and 27 is the length of its target string `libpipewire-0.3.so.0.1411.0`; `stat -L` reports the real 797568. `unshare -m` also defaults to PRIVATE propagation, so nothing could have leaked. Library rpm-intact, all three PipeWire services active, and a re-run connected. 🔑 Hiding a library in a mount namespace is a clean, contained way to test a `dlopen` fallback — just measure it with `stat -L` |
| 2026-08-31 | **P3b-ii FIELD-VERIFIED — format negotiated, 0.4.39, Asahi** | `portal-session` as **root**: daemon → privilege-dropped helper → portal session → PipeWire stream → **`negotiated BGRx 1920x1080 @ 0/1 (libpipewire 1.4.11)`** in **12 ms**. 🔑 `video_format=8` is BGRx, the order listed FIRST in our EnumFormat, so the preference was honoured — a stronger signal than “some format came back”. ⚠️ Framerate is `0/1`: GNOME declines to commit to a rate, which is why it is optional in the parse rather than a failure. **This is what validates P3b-i's POD serialisation** — bytes PipeWire accepted, not bytes I believed in |
| 2026-08-31 | **P3b-ii bug 1 — the portal session dies with its D-Bus connection** | First run: `no target node available`. The handshake looked perfect (node id, geometry, usable fd) and the node was ALREADY GONE. A portal session is owned by the connection that created it; `open_blocking` returned, the connection dropped, the portal tore the session down. `Session` now holds it in a field named `_conn` that nothing reads — deleting it “because it is unused” re-breaks capture, and the doc comment says so |
| 2026-08-31 | **P3b-ii bug 2 — SPA writes a SETTLED value as `Choice(None)`** | Second run: `the negotiated video format is not a plain id`. The diagnostic added instead of guessing answered it immediately — `object#40003 id=4 [0x1=Choice 0x2=Choice 0x20001=Choice …]`: **every** property a Choice, `mediaType` included, which can only ever be “video”. 🔑 That is the convention, not a half-finished negotiation — which is why `spa_pod_get_id` unwraps choices. `.fixed()` unwraps kind `None` ONLY; a `Range`/`Enum` still means “not settled” and returns None so the caller keeps waiting, because a range's default is a value nobody agreed to. ⚠️ `param_changed` also fires more than once |
| 2026-08-31 | **P3c-i FIELD-VERIFIED — FRAMES, 0.4.39, Asahi** | `portal-session` as **root**: `negotiated BGRx 1920x1080; 3 frame(s), MemFd stride=7680 8294400 bytes, 8291/8320 sampled bytes non-zero, checksum 0xa4dd8b6e`. Every number is internally consistent — stride = 1920x4, size = 1920x1080x4 — and **MemFd means the buffers are mmap'd, so no GBM import is needed on this host**. 🔑 The checksum CHANGES between runs (`0xe59a9e02` → `0xe459b623` three seconds later): a constant would have meant a stale or zeroed mapping, so this is live pixels off a moving screen. ⇒ **A Wayland desktop with no DRM-based capture is being captured through the portal, from the daemon.** |
| 2026-08-31 | P3c-i — why the report is shaped that way | A black frame and a working capture are BOTH “frames received”, so `frames: 3` alone would have been exactly the unfalsifiable claim this FR keeps rejecting. The report leads with `nonzero_sampled/sampled` + a checksum because those are the only fields that distinguish them. ⚠️ Sampled on a PRIME stride, not read whole: this runs in the `process` callback and a 4K frame is 33 MB. ⚠️ A buffer must be returned with `pw_stream_queue_buffer` on EVERY path — holding them starves the pool and the stream stops silently, looking exactly like a source that produced nothing. ⚠️ `spa_data.fd` is `int64_t`, not `int` |
| 2026-08-31 | **P3c-ii FIELD-VERIFIED — THE BACKEND WORKS, 0.4.40, Asahi** | `capture-smoke` as **root**, the same command that validates DRM and scrap: `capture: backend=portal (ROOMLERD_PORTAL_CAPTURE=1 … ATTENDED) width=1920 height=1080` then `delivered=5 empty=0 unchanged=0 1920x1080 stride=7680 format=Bgra mean_ms=27.70 worst_ms=34.57`. 🔑 **The dumped frame is a correct picture** — right geometry, right colours (a swapped channel order would have made the sky orange) and the cursor composited in, confirming the `CURSOR_EMBEDDED` request. ⚠️ **Honest limit: this host ALSO has DRM capture.** The portal path is proven; it is NOT yet proven on the host FR-45 was opened for (WSL2, no `/dev/dri`), which still has no ScreenCast backend |
| 2026-08-31 | **P3c-ii — a pipe and a copy, NOT `SCM_RIGHTS`** | The plan's fd-passing is the right OPTIMISATION and the wrong first version. (1) `Frame` owns a `Vec<u8>`, so the daemon copies regardless — fds would save the helper→daemon copy only, not “zero copy” end to end. (2) Passing the compositor's own buffers means NOT queueing them back until the daemon has read them: a per-frame round trip and a stall the compositor can see, and getting it wrong produces **torn frames**, which look like a codec bug. Start correct; optimise when a measurement says to. ⚠️ Both sides DROP rather than block — the helper's `try_send` runs on PipeWire's own thread |
| 2026-08-31 | **Cascade order verified — the portal does not hijack a DRM host** | Three runs on a host that has BOTH: `ROOMLERD_DRM_CAPTURE=1` → `backend=drm`; **both flags → still `backend=drm`** (DRM first, as documented); neither → the portal never engages — no helper spawned, no dialog. 🔑 Worth measuring rather than assuming: adding a backend to a cascade is exactly where an opt-in quietly becomes a default, and here the failure mode would have been an unattended host waiting on a consent dialog nobody would answer |
| 2026-09-01 | **WSL2 investigated properly — and the recorded blocker was WRONG** | FR-45 recorded WSL2 as having “no ScreenCast backend”. It can have one: **sway (wlroots) + `xdg-desktop-portal-wlr` + PipeWire** puts `org.freedesktop.portal.ScreenCast` on the bus, and the handshake runs all the way to `Start`. ⚠️ The FIRST blocker was simply that **PipeWire was not running at all** (`systemctl --user start pipewire wireplumber`) — the wlr backend refuses to initialise screencast without it and says so |
| 2026-09-01 | **⛔ WSL2 is blocked one layer DEEPER than the FR thought** | `xdg-desktop-portal-wlr`: `unable to receive a valid format from wlr_screencopy`. Cause, from sway's own log: `drmGetDevices2 failed: No such file or directory` — **no `/dev/dri` ⇒ wlroots has no renderer ⇒ screencopy can advertise no buffer format.** Reproduced on the **wayland (nested)** AND **headless** backends, and with the default AND **`WLR_RENDERER=pixman`** software renderers. 🔑 So the same missing `/dev/dri` that blocked FR-36's DRM path also blocks the PORTAL path on WSL2 — the portal was supposed to be the way around it, and it is not |
| 2026-09-01 | **⚠️ And the wlr backend has NO RemoteDesktop at all** | `wlr.portal` declares `Interfaces=…Screenshot;…ScreenCast;` only. Our detector reported it correctly — **the first field exercise of the `remote_desktop=false` branch**: “ScreenCast available but NO RemoteDesktop — capture would be read-only”. ⇒ **P4 cannot be delivered through wlr**, so WSL2 has no portal input path either, on top of having no capture |
| 2026-09-01 | P3b-i validated a second time, for free | The wlr backend logged our `SelectSources` options straight back: `types:1  multiple:0  cursor_mode:2  persist_mode:2` — exactly what we sent. That is a **second, independent implementation parsing the hand-written POD**, on a different compositor stack from the one that validated it first. The error taxonomy held too: `Ended`, not `Cancelled`, because nobody declined — the portal gave up |
| 2026-09-01 | **P4 BUILT — compile+unit-tested, NOT yet field-run** | RemoteDesktop input riding the capture session: arbiter → helper stdin (`InputMsg` JSON lines) → `Notify*`. Unit-locked: HID→evdev via the shared FR-36 table, Unicode keysyms for typed text, logical-size coordinate scaling, wheel detent accumulation, the libinput axis sign, route registration generations. Config keys `portal_capture`/`portal_input` added to the surface (the P3c env `ROOMLERD_PORTAL_CAPTURE` had NO config key — closed against the standing rule). Pending: the Asahi GNOME field run — dialog wording, devices actually granted, restore-token round trip, scroll SIGN, HiDPI logical-vs-pixel scaling on a 2× panel |
| 2026-09-01 | **P4 field run on scw-m2-asahi (aarch64, GNOME Wayland, RemoteDesktop v2) — PARTIAL, isolated to the consent gate** | Built the P4 branch on the real target (28 s). RemoteDesktop is exposed, `version=2` (persist works), `AvailableDeviceTypes=7` (keyboard+pointer present). **A/B isolation:** capture-only `--stream` RESTORED from the existing token with NO dialog and streamed BGRx 1920x1080 frames (4.96 GB in 20 s) — so the SessionKind/owner-proxy refactor did NOT regress capture; `--stream --input` reached `opening a RemoteDesktop (capture + input) session` and BLOCKED. The one difference is the fresh RemoteDesktop grant with no `-rd` token, so the block IS the one-time GNOME consent dialog — which no one was at the box to approve. ⏳ Injection landing + the restore round-trip stay UNVERIFIED behind that human approval: it is a consent grant ("Allow remote interaction with this device?"), the attended gate this whole FR is built around, and approving it is not an autonomous action. No `-rd` token was written (the run timed out before `Start` returned); the box was left on master, daemon active. ⚠️ The box's SSH host key had changed since the last session — everything else matched (aarch64, m1, the clone, the agent, the GNOME seat0 up since Aug 30), so almost certainly an sshd key regen, but worth a glance. |
| 2026-09-01 | **P4 D-Bus argument shapes PROVEN against the live GNOME RemoteDesktop v2 interface (Asahi), no grant needed** | `busctl introspect` gives every method's exact signature; all six `Notify*` tuples in `input.rs::InputContext::execute` match byte-for-byte: `NotifyPointerMotionAbsolute` `oa{sv}udd` = (session `o`, opts, stream `u`=node_id, x/y `d`); `NotifyPointerButton`/`NotifyKeyboardKeycode`/`NotifyKeyboardKeysym` `oa{sv}iu` = (…, code/sym `i`, state `u`); `NotifyPointerAxis` `oa{sv}dd`; `NotifyPointerAxisDiscrete` `oa{sv}ui` = (…, axis `u`, steps `i`). `SelectDevices` returns `o` (the request handle my `call_with_response` awaits); `AvailableDeviceTypes=7` ⊇ my requested `3` (kbd|pointer). The session is passed as a type-enforced `OwnedObjectPath` (`o`), not a string — the exact P2b object-path-vs-string bug class, correct by construction. So the injection path is now "argument shapes proven against the real interface", not merely "mapping unit-tested"; only the consent-gated LANDING (does the cursor actually move) remains. |
| 2026-09-01 | **Self-review of #1105 (high effort) — 8 findings, 4 correctness ones FIXED before merge** | (1) **zbus-in-tokio panic, live-only**: `InputContext::new` built a `zbus::blocking::Proxy` on the `#[tokio::main]` thread; zbus 5.15's `block_on` does a fresh `Runtime::block_on` ⇒ panics the INSTANT input is granted — the exact hazard `screencast::open` guards against, reintroduced unguarded, and masked in the field run because consent blocked first (the P2b lesson, exactly). Fixed: build the proxy on a joined off-runtime thread. (2) **Capture-coupling regression**: `portal_input` default ON coupled a fresh see+touch consent into every capture and blocked/fell-through if unanswered — overturning the default-ON call. Now default OFF + bounded 120s handshake wait + retry-capture-only on any WithInput failure, so input never costs capture. (3) **Multi-session route overwrite + stuck-modifier**: the single global route slot let a second viewer clobber the first (leaving it input-dead) and stranded held keys; fixed with an ordered append-only registry (oldest-active, hand-off on teardown). (4) **Partial grant**: `& mask != 0` treated keyboard-only as full input; now requires BOTH devices, degrading safely to capture-only. Cleanups: centralised `button_code`, `keysym_of` drops DEL/C1, `call_noreply` for the fire-and-forget `Notify*`. All combos compile; portal+input+core unit tests pass; fmt+clippy `--all-targets -D warnings` clean. |
