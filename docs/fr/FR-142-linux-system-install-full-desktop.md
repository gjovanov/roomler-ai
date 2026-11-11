# FR-142: A Linux system install gives the person's full desktop on its one row — a session worker under the system daemon

**Issue:** [#3680](https://github.com/gjovanov/roomler-ai/issues/3680) · **Status:** claimed 2026-11-11. Design first: nothing is designed beyond the questions below, and nothing is built
**Builds on:** FR-95 ([#2041](https://github.com/gjovanov/roomler-ai/issues/2041)) and FR-113 ([#2458](https://github.com/gjovanov/roomler-ai/issues/2458)), a system install shows Wayland and X11 desktops; FR-43 ([#971](https://github.com/gjovanov/roomler-ai/issues/971)), the macOS supervisor and its unenrolled worker
**Related:** [FR-141](FR-141-one-run-one-row.md) (the wizard's one run, whose Linux criteria are met without this FR), FR-93 P7 (a root daemon's clipboard through the session, never built), #2531

## Goal

A Linux machine installed with the system card (one root daemon, one row, the TUN mesh) gives a viewer the same
desktop a per-user install gives: clipboard both ways, the keyboard layout read and switched, the display matched to
the viewer, and desktops other than GNOME and KDE, on that same row. The operator asked for "both, under a single
row" on 2026-11-11.

## Evidence

Anchors are master `1e29eeae6` (2026-11-11). "Code" means read in the tree; "inferred" means not read.

- **What a system install lacks today** (docs, `docs/platform-support.md:97`, `:106`, `:108-109`): the person's
  desktop only on GNOME or KDE, only unlocked and on screen; no clipboard (`no-session`), no layout read or
  switch, no display match. An X11 desktop that does not report its lock to logind (XFCE, i3, xscreensaver) is
  refused with `x11_not_enabled` (`agents/roomlerd/src/capture/mod.rs:735`).
- **The clipboard says no on purpose** (code): `clipboard_reachable` is false for a root daemon with no display
  variable, and its comment says "FR-93 P7 makes the root case work through the session; until then the honest
  answer is no" (`agents/roomlerd/src/encode/caps.rs:2961-2979`). FR-93 planned P7 as "the helper pattern"
  (`docs/fr/FR-93-wayland-out-of-the-box.md:626-630`, kill switch `ROOMLERD_SESSION_CLIPBOARD=0` at `:693`). A
  grep of `agents/` and `crates/` finds that switch nowhere, and FR-93 closed on 2026-10-18.
- **A root daemon already starts itself in the person's session, for one job** (code): the portal helper runs as
  the console user with that session's variables, root dropping its privileges, the session's owner running as
  itself, anyone else refused (`agents/roomlerd/src/capture/portal/mod.rs:1407-1415`, `spawn_in_session` `:1417`).
- **The shape exists on a Mac** (code and field): FR-43's root daemon starts a worker in the person's session,
  `run --supervised --unenrolled`, and serves sessions through it over `/var/run/roomler-delegate`, field-verified
  on 0.4.200 (`docs/fr/FR-43-macos-single-enrollment.md:470-664`).
- **Not read by anyone yet**: what a Linux worker would need from the person's session on Wayland (portal or
  mutter, which need the session bus) against X11 (`DISPLAY`, `XAUTHORITY`); whether one worker per graphical
  session is enough under fast user switching; what the one-daemon rule (`docs/multi-org.md:35-51`) allows a
  worker to own.

## Design questions (P0 answers them from the code)

P0 reads these files and writes the answers into this spec, with anchors. Each answer names what it read.

| Question | Read |
|---|---|
| How does the system daemon find the person's session, and follow logins, locks and user switches? | `agents/roomlerd/src/keep_busy/logind.rs`, `agents/roomlerd/src/companion.rs` (`graphical_session`), `agents/roomlerd/src/desktop_brief.rs` |
| What does the portal helper's spawn and privilege drop give a longer-lived worker, and what does it lack? | `agents/roomlerd/src/capture/portal/mod.rs` (`run_as`, `spawn_child`, `spawn_in_session`), `capture/portal/backend.rs` |
| Which capture and input arm should a worker run per session type (X11, Wayland via mutter or the portal), against the root's DRM and `uinput`? | `agents/roomlerd/src/capture/mod.rs` (the cascade), `docs/linux-capture.md` §1.2.1, §1.2.2, §4, §5 |
| How do clipboard, layout and display match reach a session today in a per-user install? | `agents/roomlerd/src/clipboard.rs`, `agents/roomlerd/src/input/layout_x11.rs`, `agents/roomlerd/src/display_match.rs`, `agents/roomlerd/src/encode/caps.rs` |
| Can FR-43's delegation (supervisor, worker, channel, caps on the heartbeat) be reused on Linux, and what is macOS-only in it? | `agents/roomlerd/src/macos_supervisor.rs`, `agents/roomlerd/src/delegate.rs`, `agents/roomlerd/src/delegate_worker.rs`, `agents/roomlerd/src/supervision.rs` |
| What does the root daemon already serve in the person's runtime directory, and who may call it? | `agents/roomlerd/src/person_socket.rs`, #2531 |
| What does the Windows supervisor do that Linux would need (session change, respawn)? | `agents/roomlerd/src/win_service/supervisor.rs` |

P0 ends with a design section here, its kill switch named, reviewed before any code.

## Phases

| phase | what | kill switch | status |
|---|---|---|---|
| P0 | Read the files above; write the design; field read on the released daemon, recorded RED: a system install on GNOME Wayland, GNOME on Xorg, KDE, and one X11 desktop that is refused today (clipboard, layout, display match, the refusal) | read only | not started |
| P1 | The worker under the system daemon, its first desktop and features as D1 decides | a config key of the root daemon, default off (named by P0): off = today's system install byte for byte | not started |
| P2 | The rest of D1's desktops | the same key | not started |
| P3 | The default flips, on the operator's word, after the field read | the same key | not started |
| P4 | Docs: `docs/linux-capture.md`, `docs/platform-support.md` §3, the wizard's system card (with FR-141) | none | not started |

## Acceptance criteria

- [ ] **AC1**: P0's design is in this spec, every claim anchored to code read at a named master.
- [ ] **AC2**: on released builds, a system install on Ubuntu GNOME Wayland, one row: a session copies text and an
  image both ways, reads and switches the layout, matches the display. Field read, RED first on today's release.
- [ ] **AC3**: the same on GNOME on Xorg and on KDE Plasma, as far as D1 covers them.
- [ ] **AC4**: the mesh, the lock screen with `drm_capture`, and `roomler ssh` behave as before on that row.
- [ ] **AC5**: with the key off, the system install is today's, read in the field.
- [ ] **AC6**: docs updated in house style, linked from a row in `docs/README.md`.

## Open decisions

- **D1. Which sessions first?** Recommendation: GNOME and KDE on Wayland and on X11 (the four the system card
  serves today), then the X11 desktops refused with `x11_not_enabled`. Cost: those desktops wait a phase.
- **D2. What the system card says until this ships** is FR-141's D3.

## Out of scope

- The wizard's pages and the one run: FR-141.
- A per-user install with a TUN mesh, which the one-daemon rule forbids.
- The lock and login screens beyond `drm_capture`.
- macOS and Windows.

## Field log

| date | what | result |
|---|---|---|
