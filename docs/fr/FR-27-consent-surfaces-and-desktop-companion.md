# FR-27 — Host consent: every mode, every OS, and a desktop companion that works

**Issue:** [#854](https://github.com/gjovanov/roomler-ai/issues/854)
**Status:** design

## Goal

Selecting `Prompt on host (attended)` on a device must put a panel on that
device's screen — on Windows, macOS and a Linux desktop — that a human can
Approve or Deny, and while a session runs that same host must show *"Being
viewed by «name»"* with a Disconnect control. The other four consent modes must
each do what their label says, and a mode that cannot reach anybody must say so
instead of looking like a refusal.

The desktop companion is a prerequisite, not a side quest: it is the fallback
prompt surface on every platform, and today it is broken enough on macOS that
most of its pages do not function.

## Root cause / field evidence

The operator reported "screen consent seems not complete or not working" while
looking at the five-item picker in `AgentsSection.vue`. It is not one defect.
Every line below was read on `origin/master` at `861d4557`.

### Consent

1. **The owner is never prompted, in any mode.** `resolve_session_authz` returns
   `ConsentMode::Auto` for `owner_user_id == controller_user_id` *before* the
   device policy is consulted, and the UI does not say so. Every GROX device is
   owned by the operator, so the picker has never had an effect on this fleet.
   — `crates/api/src/ws/remote_control.rs:1610`
2. **A host prompt has no screen surface unless `roomler-desktop` already runs.**
   The agent writes a `.pending` marker and waits; only the companion renders it
   (750 ms LocalAPI poll). Nothing starts the companion, and the daemon never
   checks whether anything can ask. Observable failure: 30 s of nothing, then a
   deny. — `agents/roomlerd/src/signaling.rs:1884`,
   `agents/roomler-desktop/src/main.rs` (`consent_watch_loop`)
3. **"Nobody was there" is reported to the controller as "the user denied you".**
   `Decision::Timeout` maps to `granted: false`, and the hub terminates that as
   `EndReason::UserDenied`. The hub's own `ConsentTimeout` branch exists but
   loses the race every time, because the agent's prompt window is set to the
   *same* `consent_timeout_secs` the hub is waiting on.
   — `agents/roomlerd/src/consent.rs:79`, `crates/remote_control/src/hub.rs:967`
4. **`.pending` has exactly one production call site.** Fleet-RPC exec and
   Roomler SSH both prompt through the same broker and write no marker, so an
   `exec_policy` / `SshPolicy` of `prompt` can only be answered by someone who
   greps the daemon log for a request id and runs the CLI within 30 s.
   — `signaling.rs:2756`, `ssh.rs:1633` vs `git grep write_pending`
5. **`prompt_then_email` is "prompt AND email", with a 300 s on-screen modal.**
   The hub gives every owner-side mode `ASYNC_CONSENT_TIMEOUT` (300 s) and the
   agent uses that as its modal timeout. The doc-comment on
   `ConsentMode::PromptThenEmail` still describes a fallback that was never
   built. — `hub.rs:38,817`, `models.rs:382`, `state.rs:1199`

### Desktop companion

6. **Two tray icons: one with an icon and no menu, one with a menu and no icon.**
   Tauri 2.11 auto-creates a tray from `app.trayIcon` (id `main`, icon, no menu);
   `tray::install` then builds a *second* one and never calls `.icon()`. The
   blank one carries the menu — which is exactly what the operator described.
   — `tauri-2.11.2/src/app.rs:2412`, `agents/roomler-desktop/src/tray.rs:53`
7. **One bug breaks most pages on macOS and Linux: `agent_exe_path()`.** It
   probes for a *sibling* `roomlerd`, then falls back to a bare name on `PATH`.
   On macOS the companion is `/Applications/Roomler.app/Contents/MacOS/`, the
   daemon is `/usr/local/bin/roomlerd`, and a LaunchAgent's `PATH` excludes
   `/usr/local/bin` — hence the reported
   `Spawning self-update: No such file or directory (os error 2)`. The same
   failure kills apply-update, `probe_service_state`, service install/uninstall
   and the log-dir probe. — `agents/roomler-desktop/src/commands.rs:1086`
8. **macOS "Check for updates" targets the wrong mechanism.** A per-user macOS
   agent *deliberately refuses* to self-update; updates belong to the root
   `com.roomler.update` helper, woken by touching
   `/private/var/tmp/roomler-update-check`.
   — `agents/roomlerd/src/updater.rs:1588-1626`
9. **`companion::refresh_if_stale` is Windows-only** and nothing reports the
   companion's version to the server, so "Update all" cannot tell anyone the
   desktop stayed behind. — `agents/roomlerd/src/companion.rs:65`
10. **`roomler-desktop` is not shipped for Linux at all** (`scripts/install.sh:494`),
    although it already *compiles* there — `cargo clippy --workspace` installs
    webkit2gtk and walks the crate. This is packaging, not porting.
    — `.github/workflows/ci.yml:128`

### The on-screen panel

11. **"Being viewed by X + Disconnect" is Windows-only**, gated on
    `all(target_os = "windows", feature = "viewer-indicator")`, with a no-op
    `Inner` on every other OS — and `viewer-indicator` is only in the `full-hw`
    set, so the Linux and macOS release lines (`release-agent.yml:384, 621, 1922`)
    build plain `full`. There is also no LocalAPI verb for live RC sessions, so
    no thin client can render a banner or offer a Disconnect.

### Input arbitration (free-for-all vs exclusive)

The P6 stack is end-to-end complete. Four defects in it:

12. `ArbiterState::close()` never resets `mode_seeded`, so an in-session toggle
    outlives every session and the device policy stops applying until the daemon
    restarts. — `agents/roomlerd/src/input/arbiter.rs`
13. A refused floor request is silent — no broadcast, so the holder never learns
    someone asked and the requester sees nothing.
14. The participants rail renders only at `participants.length > 1`, so a single
    viewer can neither see nor change the mode.
15. Floor handover on close picks an arbitrary `HashMap` entry.

## Key design

### `PromptSurface` — native per OS, companion as fallback

`indicator::ViewerIndicator` generalises to a `PromptSurface` with two jobs:
`prompt(request) -> Decision` and `banner(show/hide)`. Backends are **probed at
daemon start and re-probed when one fails**:

| order | backend | probe |
|---|---|---|
| 1 | native `win` | existing `viewer-indicator` window class |
| 1 | native `mac` | AppKit reachable (see the main-thread note) |
| 1 | native `x11` | `DISPLAY` set **and** `x11rb::connect` succeeds |
| 2 | `companion` | `roomler-desktop` over LocalAPI, started on demand |
| 3 | `cli` | `roomler consent` sentinel (works today) |
| 4 | none | report `no_prompt_surface` rather than deny silently |

The chosen surface is logged at startup, exposed in LocalAPI `Status` and
recorded on the audit row. That is load-bearing, not telemetry: "the prompt did
not appear" is unattributable without it, which is the state this FR starts from.

The fallback chain is not a nicety either — it is the only answer for a session-0
Windows SCM host, for GNOME/KDE **Wayland** (neither exposes `wlr-layer-shell` to
arbitrary clients, so no native overlay is possible there), and for headless Linux.

Two things make the native backends much cheaper than they look, both verified
against `Cargo.lock`: **`x11rb 0.13.2`, `objc2 0.6.4` and `objc2-app-kit 0.3.2`
are already locked** (via enigo/arboard and tauri), so neither adds a new
dependency tree; and the macOS daemon **already ships as an `.app` with
`LSUIElement=true`** (`release-agent.yml:2175`), the bundle identity AppKit
windows require.

⚠️ **The macOS native backend has a prerequisite.** `agents/roomlerd/src/main.rs:653`
is `#[tokio::main]`, so the main thread is parked in `block_on` for the daemon's
whole life — the main dispatch queue never drains and AppKit can never deliver a
click to an Approve button. The macOS arm has to build the runtime explicitly, run
the daemon future on a worker thread, and `NSApp.run()` on the main thread with
`setActivationPolicy(.accessory)`. That is the riskiest change in this FR, which
is why it is sequenced last, behind its own feature and the probe.

### Consent semantics

- `AccessPolicy.prompt_owner: Option<bool>` (`#[serde(default)]`, `None` = today)
  keeps the owner shortcut but makes it visible and overridable.
- The agent-local `auto_grant_session=false` becomes a **floor** a server `Auto`
  cannot lift — the same gate-4 principle the exec and SSH designs already state:
  the device's own refusal survives the server. Implemented as a pure
  `strictest_of(directive, local)` in `consent.rs`, used by all three subsystems.
- `ClientMsg::Consent` gains `reason: Option<String>` (additive; old servers
  ignore it) so `timeout` / `no_prompt_surface` end the session as
  `EndReason::ConsentTimeout`, not `UserDenied`.
- With that in place, `prompt_then_email` clamps the *host* prompt to 30 s while
  the hub keeps its 300 s window: a host timeout no longer kills the owner's
  emailed link, while an explicit host **Deny** still ends the session.

## Phases

| # | Phase | Kill switch | Status |
|---|---|---|---|
| 0 | FR + issue + ledger (incl. the FR-24 collision repair) | n/a | in progress |
| 1 | Consent correctness — owner override, local floor, timeout≠deny, `prompt_then_email`, `.pending` for exec+ssh, `roomler consent ls` | `prompt_owner` defaults to today's behaviour; the floor only tightens | not started |
| 2 | Desktop companion — one tray with an icon, daemon-path resolution, macOS update trigger, version honesty, `ensure_running()` | each item independent; no wire change | not started |
| 3 | `PromptSurface` — 3.0 selection layer, 3.1 native Windows consent badge, 3.2 Tauri companion panels, 3.3 native X11, 3.4 native macOS | per-backend cargo feature; probe failure falls back | not started |
| 4 | Linux packaging — a **separate** `roomler-desktop` .deb | absent package = today's behaviour | not started |
| 5 | Input arbitration — mode re-seed, visible floor requests, single-viewer rail, deterministic handover | none needed (bug fixes) | not started |
| 6 | Field test on the GROX fleet | n/a | not started |
| 7 | Docs — `docs/remote-control.md` §11.2, `CLAUDE.md` known-issues | n/a | not started |

## Acceptance criteria

- [ ] With `consent_mode = prompt` and `prompt_owner = true`, controlling an
      owned device puts a panel on that device's screen and Approve starts the
      session.
- [ ] Deny, and "nobody answered", are **distinguishable at the controller**.
- [ ] A host with no reachable prompt surface reports `no_prompt_surface`; the
      controller is told nobody could be asked, and the audit row says so.
- [ ] `auto_grant_session=false` on a device defeats a server `Auto` directive.
- [ ] All five modes exercised end-to-end on the fleet, each recorded with the
      surface that served it.
- [ ] An `exec` prompt and an `ssh` prompt render on the same surface as an RC one.
- [ ] A live session shows "Being viewed by «name»" with a working Disconnect on
      Windows, macOS and a Linux X11 desktop.
- [ ] `roomler-desktop` shows exactly one tray icon, with a menu, on all three OSes.
- [ ] macOS "Check for updates" and "Apply update" both work from the companion.
- [ ] The Devices grid shows the companion version alongside the agent version.
- [ ] `free` ↔ `exclusive` verified with two concurrent viewers; the device policy
      re-applies after every session ends.
- [ ] The `roomlerd` .deb's `Depends` still contains no GTK/webkit entry.

## Deviations (accepted, recorded up front)

- **macOS: the session banner will appear in the captured stream.**
  `NSWindowSharingNone` is honoured by ScreenCaptureKit and
  `CGWindowListCreateImage`, but **not** by `CGDisplayStream`, which is what
  `capture/scrap_backend.rs` uses. True for the native panel and the Tauri one
  alike. Fixing it means moving macOS capture to ScreenCaptureKit — its own FR.
- **Wayland on GNOME/KDE gets no native overlay.** `wlr-layer-shell` covers
  sway/hyprland and is a stretch item; GNOME and KDE do not expose it to
  arbitrary clients, so those sessions use the companion.

## Open decisions

- Whether `wlr-layer-shell` is worth doing at all, or whether the companion is
  the permanent Wayland answer.
- Whether the Windows session banner should eventually move to the companion for
  a single implementation, accepting the loss of `WDA_EXCLUDEFROMCAPTURE`.
  Current answer: no.

## Out of scope

- Moving macOS capture to ScreenCaptureKit (see Deviations).
- Recording consent (`docs/remote-control.md` §11.3).
- The `remote_sessions.stats` always-zero issue.

## Field-verification log

*(empty — entries land per phase, each showing the pre-change failure first)*
