# FR-45 — Portal capture: Wayland where there is no scanout

**Issue:** [#1041](https://github.com/gjovanov/roomler-ai/issues/1041) · **Status:** P1 + P2a + P2b shipped, field-verified. P3 next · **Owner:** agent / capture

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
| **P3** | PipeWire consumer **inside the helper**, via `dlopen`: attach to the node, negotiate a format, and deliver `Frame`s to the daemon — buffer fds over `SCM_RIGHTS` once at negotiation, then a ready-message per frame — surfacing through the existing `ScreenCapture` trait as a **sixth backend**. | same |
| **P4** ⚠️ MANDATORY | Input via the portal's **RemoteDesktop** interface. Measured 2026-08-31: uinput works in WSL2 and libinput even enumerates the device — but a NESTED compositor reads its parent, not evdev, so nothing consumes the events. ScreenCast + RemoteDesktop is therefore a pair, not capture-only. | separate flag |

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
- **P3** — PipeWire consumption inside the helper, via `dlopen`. The daemon
  never links it, and after the decision above never connects to it either.
- **P4** — RemoteDesktop input in the same helper and the same session.

## Acceptance criteria

- [ ] A **nested GNOME Wayland session in WSL2** (per the WSLg + `gnome-shell
      --nested` recipe) is captured and rendered in the browser
- [ ] That session encodes with **`*_nvenc`**, and `avg_encode_ms` is within
      ~2× the 10.4 ms already measured on that host
- [x] `roomlerd capture-smoke` reports **whether the portal is available and
      why not**, on a host where it is absent — FR-36 measured a host where
      `xdg-desktop-portal` was running yet exposed **neither ScreenCast nor
      RemoteDesktop**, so availability must be *detected*, never assumed
- [ ] **The daemon still starts on a host with no PipeWire library present.**
      Verified by running it, not by inspection
- [ ] Backend order holds: a host with a real CRTC still picks **DRM**, and the
      portal is chosen only when DRM finds none
- [ ] X11/Windows/macOS unchanged; the kill switch restores the current cascade
- [x] Field-verified with the **before** state recorded beside the after — done for P2a (`no-session-bus` → `available`, with an in-session control); each later phase repeats it
- [ ] The spec and the UI both say **attended-only** — no greeter, no locked
      screen

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
