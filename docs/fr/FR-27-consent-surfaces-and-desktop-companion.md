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
   deny. — `agents/roomlerd/src/signaling.rs:2073`,
   `agents/roomler-desktop/src/main.rs` (`consent_watch_loop`)
3. **"Nobody was there" is reported to the controller as "the user denied you".**
   `Decision::Timeout` maps to `granted: false`, and the hub terminates that as
   `EndReason::UserDenied`. The hub's own `ConsentTimeout` branch exists, but
   which of the two fires is a *race*: the agent's prompt window is set from the
   same `consent_timeout_secs` the hub is waiting on.
   — `agents/roomlerd/src/consent.rs:79`, `crates/remote_control/src/hub.rs:967`

   ⚠️ **Corrected after the 2026-08-29 baseline.** This finding originally said
   the hub "loses the race every time". Measured on `mars`, it *wins*: the hub
   terminated at `t+30.000 s` and the agent's `granted=false` arrived 132 ms
   later, so an equal-window pair already reports `ConsentTimeout` today. The
   defect is real but narrower than written — it reproduces wherever the two
   windows differ, which is exactly `prompt_then_email` (agent modal 300 s) and
   any pair whose RTT lets the agent answer first. The fix is unchanged and is
   what makes the outcome deterministic instead of timing-dependent: the agent
   states its reason (`timeout` / `no_prompt_surface`) rather than leaving the
   server to infer one from a bare `granted: false`.

   ⚠️ **Corrected again after 0.4.18 (same day).** The reason field alone did
   NOT make it deterministic, because the hub's fallback timer was armed at the
   same 30 s the agent prompts for — so the agent's reasoned verdict, arriving
   ~138 ms after the window, found the session already terminated. mars on
   0.4.18 got everything right (`native=false have_surface=false`, sent
   `reason="no_prompt_surface"`) and the controller still saw "nobody
   answered". The actual fix is `consent::CONSENT_VERDICT_GRACE`: the hub
   waits 5 s longer than the window it announces, so its own timer is a
   backstop for a dead agent and never a competitor of a live one. Locked by a
   paused-clock test that replays the 138 ms gap in both directions.
4. **`.pending` has exactly one production call site.** Fleet-RPC exec and
   Roomler SSH both prompt through the same broker and write no marker, so an
   `exec_policy` / `SshPolicy` of `prompt` can only be answered by someone who
   greps the daemon log for a request id and runs the CLI within 30 s.
   — `signaling.rs:2949`, `ssh.rs:2251` vs `git grep write_pending`
5. **`prompt_then_email` is "prompt AND email", with a 300 s on-screen modal.**
   The hub gives every owner-side mode `ASYNC_CONSENT_TIMEOUT` (300 s) and the
   agent uses that as its modal timeout. The doc-comment on
   `ConsentMode::PromptThenEmail` still describes a fallback that was never
   built. — `hub.rs:39,862`, `models.rs:393`, `state.rs:1199`

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
   and the log-dir probe. — `agents/roomler-desktop/src/commands.rs:1377`
8. ~~**macOS "Check for updates" targets the wrong mechanism.**~~ **Wrong —
   corrected during implementation.** The reported
   `Spawning self-update: No such file or directory (os error 2)` made this
   look like a second, independent macOS defect. It is not: `self_update_cmd`
   has queued the root `com.roomler.update` helper for a non-root macOS caller
   since FR-5 (`main.rs:3370`), so the mechanism was already correct and
   finding 7 alone accounts for the error. What WAS wrong is that
   `cmd_apply_update` spawned detached and discarded stdout, so the daemon's
   <!-- RETIRED-NAME-ANCHOR: quoting the daemon's OWN output; that log path is
        real on every Mac in the field and FR-21 froze it. docs/fr/FR-21 -->
   "Queued for the root update helper — watch /var/log/roomler-agent/update.log"
   never reached the operator: on the one platform where the button did the
   right thing, it looked inert. Fixed by returning the output on non-Windows
   (Windows keeps the detached spawn — there the daemon hands off to msiexec
   and exits, so there is nothing to wait for).
9. **`companion::refresh_if_stale` is Windows-only** and nothing reports the
   companion's version to the server, so "Update all" cannot tell anyone the
   desktop stayed behind. — `agents/roomlerd/src/companion.rs:115`
10. **`roomler-desktop` is not shipped for Linux at all** (`scripts/install.sh:529`),
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
| 3 | `cli` | `roomlerd consent` sentinel (works today) |
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
`LSUIElement=true`** (`release-agent.yml:2406`), the bundle identity AppKit
windows require.

⚠️ **The macOS native backend has a prerequisite.** `agents/roomlerd/src/main.rs:690`
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

### P10 — the host's Disconnect ends the session for good

**The requirement** (operator, 2026-10-09): *"If 'Disconnect' is clicked on the
viewed remote desktop, it should stop the reconnect mechanism and notify the
viewer that he/she was disconnected."*

**What was wrong, measured on 0.4.119** (the #1852 log below): one click on
the banner's Disconnect → `viewee requested disconnect via overlay badge` →
`session terminated … reason=ControllerHangup` → the viewer back in a fresh
session within ~3 s. The cause is an ORDER, not a missing feature. The agent's
`kill_rx` arm (`signaling.rs`, the one arm every host-side Disconnect reaches:
the Windows badge, the companion banner over LocalAPI `RcDisconnect`) **closed
the peer first** (up to `PEER_CLOSE_BUDGET`, 5 s) and told the server after.
The browser's control data-channel `onclose` fired within milliseconds of the
close, read it as an ordinary drop, hung the old session up itself
(`rc:terminate controller_hangup` — hence the audit) and scheduled the ladder.
The agent's own `AgentHangup` arrived at a session the hub had already removed:
an idempotent no-op, so the viewer never heard who ended it. Every piece was
working as designed; they ran in the wrong order.

**Where the reason is created.** The same `kill_rx` arm, which now **says it
first**: `ClientMsg::Terminate { reason: HostDisconnect }` — a new `EndReason`,
wire `host_disconnect` (`models.rs`) — goes out before anything closes; on a
supervised Mac (FR-43) it rides the delegation link the session's answer and
ICE took (`reply_for_session`), so the worker speaks through the daemon's WS.
Control ends at the click, before the frame goes out: `AgentPeer::stop_serving`
lets the channel handlers go, stops the media pumps and removes the session
from the input arbiter, so its next event is denied and anything it holds is
released. No channel closes, so the browser has nothing to react to yet. Only
the bare connection then waits, closed in a task of its own after a grace
(`ROOMLERD_HOST_DISCONNECT_GRACE_MS`, default 1000 ms, capped at the close
budget; a failed send closes at once — nothing to wait for on a dead WS). The
banner comes down immediately. ⚠️ The grace holds the transport, never the
session's powers: the first cut kept the peer serving through it, which handed
the controller up to a second more of the machine its owner had just taken back
(found in self-review before merge).

**How it crosses the wire.** `rc:terminate`, unchanged: no new wire name, no
route, the composition baseline is untouched. The hub passes the reason through
as it always did, with one guard: a **controller** claiming `host_disconnect`
is recorded as `controller_hangup` — only the device may say the person at its
screen ended it, so the audit's word is the host's, and a pre-P10 agent can
never be echoed a variant it cannot decode. `Hub::terminate` sends the
Terminate to the controller **before** it echoes it to the agent and audits
`SessionEnded { reason }`, which is the ordering the viewer relies on.

**How the viewer decides not to reconnect.** `isRetryableTerminateReason` is an
allowlist (`agent_disconnect`, `error`), so `host_disconnect` was terminal on
day one, fail-safe. P10 adds `endedByHost(reason)` and an `endedBy` ref: the
`rc:terminate` handler sets `phase = 'closed'`, `endedBy = 'host'`, cancels the
ladder and drops `lastConnectArgs` (nothing to replay), then `teardown()`
closes the channels — with the phase already `closed`, the control DC's
`onclose` (the fast reconnect path) stays quiet. The view renders a notice,
"The person at the device ended the session.", with an explicit **Reconnect**
(`startSession`: a fresh request, through consent like any new session).
Ordinary drops are untouched: the ladder still runs on `agent_disconnect`,
`error`, a failed/disconnected peer and a closed control channel mid-session.

**Version matrix** (agent = the device, server = the hosted image, viewer = the
SPA in that image):

| viewer | server | agent | what happens |
|---|---|---|---|
| new | new | new | the notice, no auto-reconnect, audit + agent log `host_disconnect` |
| old | new | new | the viewer gets `host_disconnect`, unknown → terminal `closed`, no notice, **no reconnect** (better than today) |
| new | old | new | the old server cannot decode the frame and drops it at debug (socket stays up); the peer closes after the grace; the viewer reconnects **as today**; audit `controller_hangup` as today |
| new | new | old | the old agent closes first and says `agent_hangup` after: the browser's `onclose` wins the race **as today** |
| any | any | old + old | as today |

Locked by `pre_p10_end_reason_rejects_host_disconnect_so_old_servers_drop_the_frame`
(the "new agent, old server" cell, the way the rc.53 `Goodbye` lock documents
its own cell) and the hub's `host_disconnect_is_the_devices_claim_only`.

**Kill switch.** `ROOMLERD_HOST_DISCONNECT_GRACE_MS=0` closes the peer at once
(the pre-P10 timing; the reason still goes out). The viewer needs none: an
unknown or missing reason is terminal by the allowlist's default.

**Known limit.** A viewer whose own signalling socket is down at the moment of
the click cannot be told anything; it sees the peer die after the grace and
rides the ladder, which asks the device again when signalling returns — the
pre-P10 behaviour, for the one case where the reason has no path to travel.

### P11 — the person at a Linux system install's screen reaches the daemon (#1911)

**What was wrong, measured on 0.4.122** (the P10 Linux run below). A Linux
**system** install's root daemon binds its LocalAPI at `/var/run/roomler/roomler.sock`,
with the directory at 0700 and the socket at 0600, both owned by root. The companion
runs as the desktop user, so it read "Service offline". The banner never showed and the
Disconnect was never offered. On GNOME/KDE Wayland, where the companion is the only
surface (no `wlr-layer-shell`), consent had **no surface at all**. The root-only ACL is
right: it is the trust boundary for every administrative verb (FR-43). So the fix is not
to widen it.

**Decision (operator, 2026-10-10): a narrowly scoped socket for the person at the
device.** The other two options were a per-user worker, as on macOS (far larger, since
Linux does capture and input as root and needs no worker for media), and a
daemon-launched companion holding a one-shot secret (which misses a companion the person
started themselves).

| | What | Where |
|---|---|---|
| where | `/run/user/<uid>/roomler.sock`: the person's own `$XDG_RUNTIME_DIR/roomler.sock`, the path their companion and CLI ALREADY try first, so neither changes | `localapi::person_socket_path` |
| whose | the user of the **active** graphical session on the seat, by the same `loginctl` walk the companion launch uses. Never the display manager's greeter, never a user switched away in the background (who must not answer consent for a view of someone else's desktop), never root (root uses the daemon's own socket). It moves when that person changes | `roomlerd::person_socket` (`step`, every 4 s) |
| who may connect | the socket is MADE AS the person: bind and chmod 0600 run on a thread switched to their filesystem ids (`setfsuid`/`setfsgid`), so it is born theirs. ⚠️ Not a root bind followed by a chown: they own the dir, so they could swap the path to a symlink in between, and `chown` follows it (to `/etc/shadow`, say). Caught in self-review before merge. Each connection's `SO_PEERCRED` uid must be theirs or root | `bind_as_person`, `person_peer_ok` |
| what it serves | `Status`, `RcSessions`, `RcDisconnect`, `ConsentPending`, `ConsentDecide` and `RecordStatus`, and nothing else. Anything else is answered with a pointer to `sudo roomler …`. The match is **exhaustive with no `_` arm**, so a verb added later must be placed by hand | `localapi::person_may`, `Scope::Person` |
| what it never does | replace a LIVE socket at the path (a per-user daemon of that person owns it); remove a non-socket file; create the runtime dir (logind's); remove anything but the inode it bound | `serve_person_socket` |

The root-only `/var/run/roomler` socket is **unchanged**. A per-user daemon starts no
person socket: its own socket already is its person's. Windows (the pipe admits
Interactive Users) and macOS (the per-user worker, FR-43) are unaffected.

**Kill switch.** `ROOMLERD_PERSON_SOCKET=0` (or `false` / `off` / `no`) is the pre-P11
behaviour.

**Version matrix.** The companion and the CLI need no change: they already try the
per-user path first. An older daemon serves no person socket, so the behaviour is as
before. A CLI run as the person against a P11 daemon gets `Status`, and a clear refusal
in place of `Permission denied (os error 13)` for everything else.

## Phases

| # | Phase | Kill switch | Status |
|---|---|---|---|
| 0 | FR + issue + ledger | n/a | **done** — the FR-24 collision was already repaired on master by #850 |
| 1 | Consent correctness — owner override, local floor, timeout≠deny, `prompt_then_email`, `.pending` for exec+ssh, `roomlerd consent ls` | `prompt_owner` defaults to today's behaviour; the floor only tightens | **implemented** — timeout≠deny and `prompt_then_email` field-verified on 0.4.16, `.pending` for exec+ssh on 0.4.18, **the local floor on 2026-09-25** (AC4, WSL node, 0.4.102); `prompt_owner` still needs an owner-account run |
| 2 | Desktop companion — one tray with an icon, daemon-path resolution, update-output surfacing, version honesty, `ensure_running()` | each item independent; no wire change | **implemented** (version honesty deferred to a follow-up) — field pending |
| 3 | `PromptSurface` — 3.0 selection layer, 3.1 native Windows consent panel, 3.2 Tauri companion panels, 3.3 native X11, 3.4 native macOS | per-backend cargo feature; probe failure falls back to the companion | **all implemented** — field pending. ⚠️ `viewer-indicator-macos` is compiled by CI but deliberately **NOT in the macOS release feature set**: it moves tokio off the main thread, i.e. changes how every Mac agent STARTS, and macOS updates are owned by the root helper — a daemon that fails to start cannot pull its own fix. Enable only after a dispatch artifact runs on a real Mac. *FR-97 P2b (2026-10-21) ran that procedure on a tart Mac (both halves, a viewer from outside): the native panel came up, Approve connected and Deny declined, so the feature is in the macOS release set (`.github/workflows/release-agent.yml:2644`); the field Mac and a release follow* |
| 4 | Linux packaging — a **separate** `roomler-desktop` .deb | absent package = today's behaviour | **implemented** — field pending |
| 5 | Input arbitration — mode re-seed, visible floor requests, single-viewer rail, deterministic handover | none needed (bug fixes) | **implemented** — field pending |
| 6 | Field test on the GROX fleet | n/a | **partly done, 2026-08-29 on 0.4.16** — the Windows native panel is verified end to end and the field log below lists exactly what is not. The test itself found 3 defects (#877), one of which froze the whole pre-0.4.16 Linux fleet |
| 8 | Field fixes — release-asset ordering, the virtual-desktop guard, the CLI name, and phase 2d's `companion_version` | the ordering fix is server-only and additive; the x11 guard only ever DECLINES | **deployed** (#877; API `v20260829-0d5078f44e42`, agents ≥ 0.4.18) — the guard and the ordering were field-verified 2026-08-29; `companion_version` read on the live grid 2026-09-25: on the wire for all 21 devices, rendered only on a skew, and the fleet has none (AC10 half; see the log) |
| 7 | Docs — `docs/remote-control.md` §11.2, `CLAUDE.md` known-issues | n/a | **done** — §11.2 rewritten (76bd6ef6) and the 2026-04-17 known-issue replaced rather than deleted; the resolution diagram + the floor's field line added to §11.2 on 2026-09-25; the docs criterion is in the list below |
| 9 | The companion banner's Windows manners (macOS, Linux) — shown **without taking focus** (`orderFrontRegardless`; on macOS `show()` made it KEY, so the first click into the person's own window after every session start was lost), hides 2.5 s after the pointer leaves (after a 4 s first show), comes back after a 1.2 s rest at the top edge, stays while anything records, and a 2 px red frame round the screen for the session (macOS). Docs: `docs/desktop-companion.md` §12 | `ROOMLER_DESKTOP_BANNER_AUTOHIDE=0` (banner stays, as before), `ROOMLER_DESKTOP_FRAME=0` (no frame) | **field-verified on the Mac, 0.4.117, 2026-10-08** (field log) — every clause held; the reveal logic's 6 unit tests stand |
| 10 | **The host's Disconnect ends the session for good** — the agent says it FIRST (`host_disconnect`, a new `EndReason`) and closes the peer after a grace; the hub forwards it to the controller as the device's word only; the viewer shows "The person at the device ended the session." with an explicit Reconnect and does not auto-reconnect; ordinary drops still do. Design + version matrix above. Docs: `docs/remote-control.md` §11.5, `docs/desktop-companion.md` §12 | `ROOMLERD_HOST_DISCONNECT_GRACE_MS=0` (close at once, the pre-P10 timing); the viewer needs none — an unknown reason is terminal | **shipped** — #1886 in agent **0.4.121** + `hosted-20261009-cac4761` (promoted 2026-10-09 07:42Z). RED on record: 0.4.119 + `hosted-20261005-d2dc0ef`, 2026-10-08 14:38:43Z. **GREEN on all three surfaces, 2026-10-09** (the field log). The Mac's banner: the reason at the viewer 72 ms after the click, 65 s held with no request, Reconnect explicit, an ordinary drop back in 6.3 s with no click. The Windows badge: told the server 1 ms after the click, 67 s held, Reconnect explicit, an ordinary drop back in 0.95 s. The Linux banner, where the companion reaches its daemon: 33 ms, 65.8 s held, 0.54 s Reconnect, 4.0 s drop. ⚠️ A Linux SYSTEM install's companion cannot reach its root daemon at all, so it has no banner and no Disconnect: #1911, which P11 fixes |
| 11 | **The person at a Linux system install's screen reaches the daemon (#1911).** A socket in the person's own runtime dir, 0600 and theirs, admitting them or root (`SO_PEERCRED`), serving only the safety surface: status, the viewing sessions, their Disconnect, consent, and the recording state. It follows the active graphical session's user. The daemon's root-only socket is unchanged. Design above. Docs: `docs/desktop-companion.md` §12, `docs/security-baseline.md`, `docs/installation.md` | `ROOMLERD_PERSON_SOCKET=0` (the pre-P11 behaviour) | **field-verified on the 0.4.124 candidate** (2026-10-10, the log): RED on the released 0.4.123, then GREEN on the same vmtest system install after the upgrade, with the daemon's socket untouched and the companion left on 0.4.123. Ships in 0.4.124, whose tag waits on the operator's call on #1915, which rides with it |

## Acceptance criteria

Ticked only where a run is recorded in the field log below.

- [x] With `consent_mode = prompt`, controlling a device puts a panel on that
      device's screen and Approve starts the session — **Windows**, 0.4.16.
      **The `prompt_owner = true` half: GNOME on Wayland, the lab's Fedora Asahi
      box, agent and API 0.4.158, 2026-10-23**, with the controller signed in as
      the device's owner. Unset → `effective_mode=AutoGrant`, nothing on screen;
      set → the companion's panel (`native=false`, as designed there), Approve →
      `Prompt { timeout: 300s } granted=true` → connected; Deny → `UserDenied`;
      restored → `AutoGrant` again. Set through the API, not the dialog. See the
      2026-10-23 log.
- [x] Deny, and "nobody answered", are **distinguishable at the controller**.
- [x] A host with no reachable prompt surface reports `no_prompt_surface`; the
      controller is told nobody could be asked, and the audit row says so —
      **mars, 0.4.18 + API `v20260829-507d7b33990a`** (it took both halves: the
      agent's virtual-desktop guard AND the hub's verdict grace; see the log).
- [x] `auto_grant_session=false` on a device defeats a server `Auto` directive —
      **NEO16-WSL (the WSL node), agent 0.4.102, API 0.4.101, 2026-09-25.** The
      control first: same account (the owner), same device, same server policy
      (`consent_mode = auto`), local setting at its as-found `true` →
      auto-granted in **1.5 ms** (`effective_mode=AutoGrant`). With the floor
      set, the identical request ran as a prompt over the server's **300 s**
      window, found no surface (virtual desktop, no companion) and ended
      `reason="no_prompt_surface"` → hub `NoPromptSurface` 41 ms later; the
      controller was told nobody could be asked. Both settings restored and
      re-proven (auto-granted again, 0.3 ms). See the 2026-09-25 log.
- [x] All five modes exercised end-to-end on the fleet, each recorded with the
      surface that served it — **done 2026-10-24**, the last three on the lab's
      Fedora Asahi box (GNOME on Wayland) with the owner as the controller:
      - `auto`: no surface (the controls in the 2026-10-23 log).
      - `prompt`: the Windows native panel (0.4.16); the companion's panel on GNOME
        Wayland (2026-10-23).
      - `push`: the owner's in-app notification and its `/consent/<token>` page
        (2026-10-23, while the deployment had no push service). Since 2026-10-24
        there is also the **web-push card**: a browser that opted in showed it
        seconds after the request (the 2026-10-24 log).
      - `email`: the owner's real inbox. SendGrid sent it, the owner confirmed it
        arrived, the email's link opened `/consent/<token>` with no referrer, and
        Approve connected the session (2026-10-24). An unanswered run timed out at
        5 min as `consent_timeout` (2026-10-23).
      - `prompt_then_email`: the host half earlier; the owner half on 2026-10-23,
        when the host prompt timed out at 30 s, the session waited, and the
        owner's Approve connected it.
      See the 2026-10-23/24 log. The controller's wording for an owner-side
      mode used to speak of "the person there". Since #2810, the viewer is
      told who is asked (`consent_asks` on `rc:session.created`) and words its
      wait and the outcome for that person.
- [x] An `exec` prompt and an `ssh` prompt render on the same surface as an RC one
      — **2026-10-24, the lab's Fedora Asahi box (GNOME on Wayland), agent
      0.4.162.** The gates were opened only for the run, then closed and
      re-proven closed. The command went to the person at the screen, not root.
      All three prompts came from the companion, on the same surface as the RC
      one in the 2026-10-23 log:
      - *Command execution request*, with the command shown → Approve → it ran
        (`wlab`, exit 0);
      - *SSH session request* → Approve → the one command ran (`wlab`, exit 0).

      The earlier blocker (`EXEC_DEVICE` / `SSH_DEVICE`) was no longer one: the
      controller holds both. Found on the way, #2815: each request is shown
      twice, the exec prompt names the org "primary", and the SSH prompt shows
      a raw identity. See the 2026-10-24 log.
- [x] P9, on a Mac: a session start brings the banner up without taking focus (the
      next click into the person's own window acts), it hides 2.5 s after the
      pointer leaves (after its 4 s first show), returns after a 1.2 s rest at the
      top edge, stays while a recording runs, and a red frame surrounds the screen
      for exactly the session's duration — **the Mac, agent 0.4.117, 2026-10-08**,
      every clause (see the log): banner up at +1.7 s, hidden at +6.2 s, back at
      +14.5 s (and again at +24.9 s) on a top-edge rest; the window census read the
      banner on-screen through a recording with the pointer away; the red frame on
      every frame of the session and gone from the census afterwards; and the next
      click into TextEdit after a fresh banner landed its keystrokes.
- [x] A live session shows "Being viewed by «name»" with a working Disconnect on
      Windows, macOS and a Linux X11 desktop. (Windows border confirmed visible +
      capture-excluded during a session; **the macOS half is done — the Mac,
      0.4.117, 2026-10-08**: the banner reads "Being viewed by «owner name»" and
      clicking its Disconnect logged `viewee requested disconnect via overlay
      badge` → `session terminated … reason=ControllerHangup`. ⚠️ On 0.4.117 the
      banner window was not key, so the first click only activated the companion
      and a following click triggered Disconnect (no `accept_first_mouse`); **#1852
      fixed that (#1854, agent 0.4.118, carried by 0.4.119)** — one click on
      0.4.119, 2026-10-08 (the log below). The Windows reveal-on-hover badge and
      the Linux X11 panel are still untested.) *(2026-10-14, FR-93 P6: the Linux
      X11 consent PROMPT was field-run on GNOME 50 and Plasma 6.7, through
      Xwayland, on 0.4.135. It showed and took the click, then stayed on screen
      after it, frozen: its unmap was never flushed; fixed in FR-93 P6a. The
      banner half of this criterion is the companion's, and still untested on
      X11.)* **The Linux X11 half: GREEN, 2026-10-25, agent and companion
      0.4.169** on Ubuntu 24.04 with GNOME 46 on Xorg (the log). The banner tucked
      away and came back on a top-edge rest, and one click on its Disconnect ended
      the session for good. **The Windows badge's text: GREEN, 2026-10-25,
      0.4.169** (the log). It is capture-excluded by design, so it was read from
      the screen itself: a Win11 VM's framebuffer through the hypervisor
      (`virsh screenshot`), with the pointer moved on the VM's tablet. After a
      top-edge rest the badge read "Being viewed by" / "vmtest admin", with a
      Disconnect. One click on it ended the session for good, re-proving 10-09's
      hardware click. **Met on all three: macOS 10-08, Linux X11 and Windows
      2026-10-25.**
- [x] `roomler-desktop` shows exactly one tray icon, with a menu — **Windows**,
      measured 2 → 1 against the 0.4.15 build. macOS/Linux untested.
- [ ] macOS "Check for updates" and "Apply update" both work from the companion.
      **"Check for updates" verified — the Mac, 0.4.117, 2026-10-08**: the button
      runs `<daemon> self-update --check-only` (`cmd_check_update`, via
      `agent_exe_path`), replicated as the console user → `Up to date (current:
      0.4.117, latest: agent-v0.4.117)`, exit 0. "Apply update" has **nothing to
      apply** (the Mac is on the latest release), so its success cannot be shown
      here; the macOS update MECHANISM itself was proven end to end by the 0.4.109
      canary install (the FR-85 log). Left unticked until an update is pending.
      *(2026-10-25: every Mac in the fleet was already on 0.4.169, daemon and
      companion. The window is between a release publishing and the fleet roll:
      at the next release, leave one Mac out of the roll and click "Apply update"
      in its companion.)*
- [x] The Devices grid shows the companion version alongside the agent version.
      (Implemented in #877; deployed since API `v20260829-0d5078f44e42` + 0.4.18.)
      **Half met, 2026-09-25 on the live grid (API 0.4.101, 21 devices):** the
      value is on the wire for every row — 10 companion hosts (Windows, macOS)
      report a version equal to the daemon's, 11 report *absent* (Linux hosts
      without a companion, and pre-FR-27 agents) — and the deployed bundle
      renders `· desktop v<x>` in warning colour with its tooltip the moment a
      row's companion version differs (shown by injecting a skew into the client
      store on the live page). **Missing:** the grid renders the companion
      version ONLY when it disagrees with the daemon's (by design,
      `AgentsSection.vue:354`), no device in the fleet carried a skew today, so
      the caption has not been seen on a real row — and per #1617 the value is
      the *installed* version, so the one skew that actually happened (a 0.4.92
      companion still RUNNING under a 0.4.101 daemon) was invisible to it.
      **Seen on a real row, 2026-10-25, agent 0.4.170** (the log): the lab's
      Fedora box with its companion package one release behind its daemon. The
      live grid read `v0.4.170 · desktop v0.4.169` in warning amber, with the
      tooltip; every other row showed nothing. The running-versus-installed gap
      closed meanwhile: #1617 restarts a running companion after a macOS update,
      and FR-117's sync on Linux restarts it when it moves the package
      (`restarted=1`).
- [x] `free` ↔ `exclusive` verified with two concurrent viewers; the device policy
      re-applies after every session ends. **Second half done** (re-seed proven on
      one daemon process, no restart). **Two-viewer half done 2026-10-24**, on the
      lab's Fedora Asahi box (agent 0.4.162), with Edge and Chrome on ONE PC as the
      two viewers:
      - both connected ("2 viewers on this device (input: free)");
      - a switch to `exclusive`;
      - an idle takeover;
      - a request refused while the holder was active, which reached both ends:
        WAITING… for the requester, "… wants control · GRANT · IGNORE" for the
        holder;
      - the GRANT;
      - the floor back to the remaining viewer when the holder left;
      - back to `free`.

      Every step is matched by an `input arbiter:` line. The old blocker was a
      second viewer in the same BROWSER, not on the same machine. See the
      2026-10-24 log.
- [x] The `roomlerd` .deb's `Depends` still contains no GTK/webkit entry —
      `libasound2, libc6, libxcb-randr0, libxcb-shm0, libxcb1`, and the guard that
      asserts it actually runs now (#871).
- [x] **Docs** — `docs/remote-control.md` §11.2 documents the consent model in
      the house style (the five modes and their windows, the owner shortcut, the
      device's floor, the surface chain, the verdict grace) with a mermaid
      diagram of how one session's consent is resolved, and is linked from
      `docs/README.md` (the docs-before-close rule). The field-result half of
      phase 6 lives in this spec's log.
- **P10** — one click on the host's Disconnect ends the session for good: the
  viewer shows that the person at the device ended it and stays disconnected for
  ≥ 60 s with no new session in the agent log; Reconnect is an explicit click
  that goes through consent; an ordinary drop (the viewer's signalling socket
  cut mid-session, even after a host Disconnect and a Reconnect) still
  auto-reconnects; the agent log and the session audit record `host_disconnect`,
  not `controller_hangup`. Docs: `docs/remote-control.md` §11.5 and
  `docs/desktop-companion.md` §12. One box per surface, ticked as the log
  records it:
  - [x] the companion banner (macOS), from a Playwright field spec on the live
        site — GREEN 2026-10-09 on 0.4.121 + `hosted-20261009-cac4761`
  - [x] the Windows badge, which is capture-excluded, so the click is aimed by
        its fixed geometry through the viewer (Claude in Chrome on the live
        site) — GREEN 2026-10-09 on 0.4.121 + `hosted-20261009-cac4761`
  - [x] the companion banner (Linux), on a vmtest Ubuntu 24.04 GNOME Wayland
        guest — GREEN 2026-10-09 on 0.4.122. It ran wherever the companion
        reaches its daemon, which a per-user install provides. On a SYSTEM
        install it cannot (a root-only socket, #1911); the run opened the socket
        on the throwaway guest to exercise the banner itself
- **P11** — on a Linux **system** install with the daemon's socket untouched,
  the person at the screen reaches what is theirs and nothing else (#1911):
  - [x] the companion reads "Connected", not "Service offline", and as the
        person `roomler status` answers — **the 0.4.124 candidate, 2026-10-10**
        (the log): "● Connected" in the stream, where 0.4.123 read "Service
        offline", on the unchanged 0.4.123 companion
  - [x] the banner shows for a session, and its one-click Disconnect ends it as
        `host_disconnect` (the P10 checks) — `host_disconnect` 32 ms after the
        click, no break-glass beside the notice, 65.8 s held with no request,
        the audit, Reconnect in 566 ms
  - [x] as the person, an administrative verb (`roomler peers`, a `config set`)
        is refused with the pointer to `sudo`, and the daemon's own
        `/var/run/roomler` socket is still 0700 / 0600 root — both verbs
        refused with the pointer; the root socket unchanged
  - [x] the socket is the person's (0600, their uid), and it is gone after
        they log out; it is never served for the greeter or for root —
        `srw------- vmtest:vmtest`. It was gone 18 s after the logout,
        removed by the daemon (the runtime dir survived). There was none in
        the greeter's runtime dir. Root at the seat is by construction (`step()`
        ignores uid 0, locked by `the_socket_follows_the_person_at_the_seat`);
        GDM on this image refuses a root login, so it is not field-tested
  - [x] docs: `docs/desktop-companion.md` §12, `docs/security-baseline.md`,
        `docs/installation.md`, and a row in `docs/README.md` if a new page is
        added — all three in #1918; no new page, so no index row

## Deviations (accepted, recorded up front)

- **macOS: the session banner will appear in the captured stream.**
  `NSWindowSharingNone` is honoured by ScreenCaptureKit and
  `CGWindowListCreateImage`, but **not** by `CGDisplayStream`, which is what
  `capture/scrap_backend.rs` uses. True for the native panel and the Tauri one
  alike. Fixing it means moving macOS capture to ScreenCaptureKit — its own FR.
  P9's red frame is in the stream too: the controller sees a red frame round a
  Mac's screen during a session (accepted by the operator, 2026-10-06).
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

### 2026-08-29 — 0.4.15 baseline, then 0.4.16

Full write-up on the issue ([baseline](https://github.com/gjovanov/roomler-ai/issues/854#issuecomment-5462090974) ·
[result](https://github.com/gjovanov/roomler-ai/issues/854#issuecomment-5462384972)). Server
`v20260829-673a1686220f`; controller is an ADMINISTRATOR who is **not** the devices' owner,
so `resolve_session_authz` takes the admin-without-override arm and the device's mode applies.

| # | finding | 0.4.15 | 0.4.16 |
|---|---|---|---|
| 2 | on-screen prompt (Windows) | no consent window exists at all — only the two pre-existing indicator windows, both `vis=False`, for the whole 30 s | `RoomlerConsentWClass` `vis=True`, `460x232` at `(1050,48)`, `GetWindowDisplayAffinity = 0x11` **while shown** |
| 3.0 | which surface served it | nothing logged | `consent prompt surface … native=true have_surface=true` |
| — | Approve | — | human clicked at t+14.7 s → `allow=true` → `outcome=Granted`, session started |
| 3 | Deny vs timeout | — | `outcome=Denied` → *"Someone at that device declined the request."*; `reason="timeout"` → *"Nobody answered the prompt on that device in time."* |
| 5 (1d) | `prompt_then_email` host window | agent modal ran 300 s and its timeout killed the emailed link | panel up **exactly 30 s** (`timeout_secs=30`), then hidden — and the session was still `awaiting_consent` at t+59 s, i.e. the hub kept the owner's window open |
| 6 | tray icons | **two** `tray_icon_app` windows | **one** |
| 12 (5a) | in-session mode outlives the session | exclusive → last viewer leaves → reconnect ⇒ still `exclusive` | same sequence, no daemon restart ⇒ back to the device policy's `free` |
| 14 (5c) | rail at one viewer | hidden | *"Viewers · input free"* + the holder + **Switch to exclusive input** |

⚠️ **Three defects the field test found**, all fixed in #877 — see the issue comment. The
worst was self-inflicted: publishing a second Linux `.deb` in the agent release froze every
pre-0.4.16 Linux agent, because their picker takes the FIRST `.deb` for their arch and
`/api/agent/latest-release` forwards GitHub's order. The other two are that a
<!-- RETIRED-NAME-ANCHOR(3): a record of what was WRONG. The env spelling is
     the one those hosts set (read as a fallback by indicator/x11.rs), and the
     retired binary name is the defect being described — rewriting either
     would make the log describe something that did not happen. -->
`ROOMLER_AGENT_VIRTUAL_DESKTOP=1` host wrongly counted as a consent surface, and that the one
line a headless operator gets still named the FR-21-retired `roomler-agent`.

⚠️ **Not verified, and why**

- `prompt_owner` — **not exercisable from this account**: every Grox device is owned by a
  different user id, so the toggle never renders. Server side is covered by the
  `resolve_session_authz` tests; the UI half needs one run signed in as the owner.
- `no_prompt_surface` — there is **no clean negative control left in the fleet**: mars,
  jupiter and the WSL node all run a virtual desktop, and it took the #877 fix to make any of
  them decline. Re-test on 0.4.17.
- **two concurrent viewers** — a second viewer from the SAME browser to the same device
  never gets media (*"No video from this device after 3 attempts"*), reproduced against both
  NEO16 and mars. Same-host artifact rather than a P6 defect on the evidence available, but
  it means `free` ↔ `exclusive` with two live viewers is still unproven; it needs a second
  controller machine.
- `email` / `push` — both resolve at the **owner**, who is a different account here.
- macOS and Linux-X11 panels, exec + ssh prompts, macOS Check-for-updates.

### 2026-08-29 — 0.4.18 on mars: the negative control, half right

`agent-v0.4.18` (#877's agent half) rolled to mars, jupiter and NEO16. On mars:

| | observed |
|---|---|
| daemon start | `no native consent panel on this host — … this host's X display is the daemon's own virtual desktop` — the guard declined the Xvfb |
| prompt | `consent prompt surface … native=false have_surface=false` — correct |
| agent verdict | `decision=Timeout … reason="no_prompt_surface"` at **t+30.138 s** — correct |
| hub | `session terminated by server reason=ConsentTimeout` at **t+30.000 s** — 138 ms EARLIER |
| controller | *"Nobody answered the prompt on that device in time."* — the wrong sentence |

So the agent is right and the controller is still told the wrong thing: the hub's
fallback timer and the agent's window were the same number. Fixed server-side
(`CONSENT_VERDICT_GRACE`, #892).

### 2026-08-29 — API `v20260829-507d7b33990a` (#892): the negative control, whole

Same host, same policy, same click — with the hub now waiting 5 s past the window:

```
14:47:13.494  consent prompt surface … native=false have_surface=false
14:47:43.630  consent decision → sending rc:consent … granted=false reason="no_prompt_surface"
14:47:43.631  session terminated by server … reason=NoPromptSurface          ← 1 ms AFTER the verdict
```

Controller, at t+30.3 s: *"That device could not show a prompt to anyone — nobody
is signed in at its screen, or the Roomler desktop app is not running there. Start
it on the device, or set this device to email/push consent."* Before #892 the
terminate line read `reason=ConsentTimeout` and came 138 ms BEFORE the verdict.

Every device policy touched during the run is restored (mars back to unset).

Also on 0.4.18: `companion_version` is live end to end — NEO16 reports `0.4.18`
(the sidecar the daemon's own refresh wrote), mars reports *absent* (no
companion, and correctly not an empty string). The release-time picker guard
ran for the first time on a real tag and printed the daemon `.deb` for both
arches.

### 2026-09-25 — AC4 on the WSL node: the local floor holds against a server `auto`

Device **NEO16-WSL** (display name; Linux, the root `roomlerd.service` unit,
virtual desktop, no companion), API `0.4.101`. The controller is the device's
**owner**, so `resolve_session_authz`
(`crates/modules/remote/src/controller.rs:448`) resolves `auto` by the owner
shortcut; the device's server policy was additionally set to `consent_mode =
auto` for the run (as found: unset) so the directive did not rest on the
shortcut alone. Nothing in the consent code differs between the two agent
builds below (`git log agent-v0.4.101..agent-v0.4.102` touches no consent
file), and the hub sends `consent_timeout_secs = host_prompt_timeout_secs =
300` for `auto` and `prompt` alike (`crates/modules/fleet/src/hub.rs:1058`,
`crates/remote_control/src/consent.rs:115`).

| | control (as found) | with the floor | restored |
|---|---|---|---|
| agent build | 0.4.101 | 0.4.102 | 0.4.102 |
| `auto_grant_session` (device) | `true` | **`false`** | `true` |
| server directive | `auto` | `auto` (unchanged) | `auto` |
| broker at daemon start | `mode=AutoGrant` | `mode=Prompt { timeout: 30s }` | `mode=AutoGrant` |
| verdict | `Granted`, **1.5 ms** | `Timeout` after **300.1 s**, `reason="no_prompt_surface"` | `Granted`, 0.3 ms |
| hub end reason | — (session ran) | `NoPromptSurface`, **41 ms after** the verdict | — (session ran) |
| controller | connected | `awaiting_consent` for 5 min, then *"That device could not show a prompt to anyone…"* | connected |

The floor run, verbatim from the daemon (UTC; hostname prefix stripped):

```
13:04:20.785  incoming session request — running consent broker … consent_timeout_secs=300 … consent_mode=Prompt { timeout: 30s } org=Some("Grox")
13:04:20.785  no desktop companion on this host — an on-screen prompt is not possible; answer with `roomlerd consent --list` / `--approve`, or set this device to email/push consent
13:04:20.785  consent prompt surface session=6ab6714c… native=false have_surface=false
13:04:20.785  operator consent required — answer it with `roomlerd consent --session 6ab6714c… --approve|--deny` … timeout_secs=300
13:09:20.923  operator consent decision session="6ab6714c…" outcome=Timeout
13:09:20.923  consent decision → sending rc:consent session=6ab6714c… decision=Timeout effective_mode=Prompt { timeout: 300s } granted=false reason="no_prompt_surface"
13:09:20.964  session terminated by server session_id=6ab6714c… reason=NoPromptSurface
```

The attribution is in the two windows. The device's own mode is `Prompt {
timeout: 30s }` (line 1 prints `consent_broker.mode()`,
`agents/roomlerd/src/signaling.rs:2562`), yet the prompt stood for **300 s** —
the server's window — which is exactly the shape `strictest_of`
(`agents/roomlerd/src/consent.rs:136`) produces when it downgrades an `auto`
directive: `Prompt { timeout: host_window }` (`signaling.rs:2526`). The control
run pins the other half: same account, device and policy, only the local
setting differs, and it auto-granted (`decision=Granted effective_mode=AutoGrant`,
1.5 ms after the request). Meanwhile `roomlerd consent --list` on the device
listed the outstanding `[rc]` request from the controller with its approve
command — the CLI surface (chain step 3) was live for the whole five minutes;
nobody used it, which is the point of the test.

Controller, at t+5:00: *"That device could not show a prompt to anyone - nobody
is signed in at its screen, or the Roomler desktop app is not running there.
Start it on the device, or set this device to email/push consent."*

⚠️ **Two things the run found on the way.**

- **Restarting the daemon for the floor let its auto-updater pull
  `agent-v0.4.102`** (`new release available — spawning installer and exiting`,
  then a systemd restart 1 min 40 s later). The first floor attempt landed in
  that gap and the controller was told *"This device is offline"* —
  `agent_offline`, not a consent outcome; the run above is the retry. The
  device stays on 0.4.102: a self-update is the fleet's normal behaviour, not a
  change to roll back.
- **`GET /api/tenant/{tid}/session/{sid}/audit` answers 500 on API 0.4.101** —
  for every session tried, including ones a parallel session had run on the
  same device earlier that day:
  `{"error":"internal","message":"Kind: numeric permissions are not accepted here …"}`.
  The audit rows' `permissions` are written as a number
  (`crates/remote_control/src/audit.rs:129,169`, via `bson::to_bson`) and read
  back through a path that expects the pipe-separated string — the
  `is_human_readable` split. So the server's own `ConsentPrompted` /
  `NoPromptSurface` record for these sessions exists but is unreadable through
  the API; the attribution above is from the agent's log and the controller,
  not the audit row. A separate defect that needs its own fix. (The session
  GET, `/session/{sid}`, works, and carries no `consent_mode`.)

Restoration, verified: `/etc/roomler/config.toml` back to
`auto_grant_session = true` (the only other diff against the as-found copy is
the daemon's own `last_known_good_version`, `0.4.101 → 0.4.102`), broker
`mode=AutoGrant` on the restart; the server policy PUT back to its as-found
object (`consent_mode: null`, `prompt_owner: null`, empty allowlists) and read
back equal; one more session auto-granted in 0.3 ms and was hung up.

### 2026-09-25 — AC10 on the live grid: on the wire everywhere, rendered only on a skew

The Devices page, API `0.4.101`, 21 devices. Each row's caption reads
`id: … · <machine_id> · v<agent_version>`
(`ui/src/components/admin/AgentsSection.vue:353`); the companion half,
`· desktop v<companion_version>`, renders **only when the companion disagrees
with the daemon** (`companionSkew`, `AgentsSection.vue:362-366,1826`), and the
card view's chip follows the same rule (`:697`).

| device (display name) | daemon | `companion_version` on the wire | caption shows |
|---|---|---|---|
| NEO16 (Windows, companion) | 0.4.101 | `0.4.101` | `· v0.4.101` — no `desktop` half (equal) |
| MacBook-1 (macOS, companion) | 0.4.102 | `0.4.102` | `· v0.4.102` — no `desktop` half (equal) |
| NEO16-WSL (Linux, no companion) | 0.4.101 | *absent* | `· v0.4.101` |
| mars (Linux, no companion) | 0.4.101 | *absent* | `· v0.4.101` |
| fleet-wide | — | 10 equal · 11 absent · **0 skewed** | no row shows a `desktop` half today |

With a skew injected into the client store on the live page (NEO16-WSL's row
given `companion_version = "0.4.99"`; no server call), the same row rendered
`id: … · v0.4.101 · desktop v0.4.99` in warning colour (`rgb(255,193,7)`) with
the tooltip *"roomler-desktop is on v0.4.99 while the daemon is on v0.4.101 —
the companion updates separately …"*; reverting the store removed it. So the
deployed rendering path works; what has not been seen is a real skew, and the
fleet has none today.

⚠️ **The value is the installed version, not the running one** (#1617, the same
day): a Mac ran a 0.4.92 companion under a 0.4.101 daemon for 17 days while this
field read `0.4.101` throughout. The grid can only ever surface a skew the
companion's *files* have; a stale *process* needs the companion to report
itself (over LocalAPI) — a follow-up, not this FR's phase 2d.

### 2026-10-08 — 0.4.117 on the Mac: P9, the banner's Disconnect, Check-for-updates

Agent 0.4.117 (the P9 code shipped in #1819). The controller is the device's
owner (`auto_grant_session`), so every session auto-granted. Sessions were
driven from the demo harness (`scripts/record-demo.sh` + its scripted scene),
which captures the viewer's own frames; on macOS the banner and the red frame
are IN the `CGDisplayStream` capture (a recorded deviation), so the frames show
them. The two things the stream cannot show were read straight off the Mac's
window server with a read-only `osascript` **window census**
(`CGWindowListCopyWindowInfo`, on-screen windows owned by a Roomler process):
whether the banner and frame were present, and that they were gone once the
session ended.

**P9, clause by clause** — one scripted fullscreen session, the pointer moved by
the new `move` step; frames read by a detector over the banner box and the four
screen-edge strips:

| clause | observed |
|---|---|
| shown at session start, no focus taken | banner up from +1.7 s; the census during the session had the banner at window layer 5 (a non-activating panel) while the menu bar stayed on the person's own app (TextEdit) throughout |
| hides 2.5 s after the pointer leaves, after the 4 s first show | banner down at +6.2 s (pointer away since the fullscreen click) |
| returns after a 1.2 s rest at the top edge | back up at +14.5 s after a top-edge `move` at +13.6 s, and again at +24.9 s after a second one at +24.0 s; down again 3.0 s after the pointer left the top edge (`away` +18.3 s → +21.3 s), the one observation that times the 2.5 s hide itself. Each reveal came 0.9 s after its move's mark, shorter than the 1.2 s dwell: the harness's marks and frame times are not aligned that finely, so this run shows the reveal, not its exact delay |
| stays while a recording runs | in a second session a remote recording ran ~87 s (`h264_videotoolbox`, native 3024×1964); the census had the banner on-screen at 0:03, 0:38 and 1:21 with the pointer parked at the top-right menu — far from the banner and well past the 4 s + 2.5 s window. It had auto-hidden and the recording brought it back (`tray::recording_now`). The A/B is the first session, which auto-hid at +6.2 s with no recording |
| red frame for exactly the session's duration | the red frame was on every fullscreen frame of the live session, which did not end cleanly: the stream carried one frame between +32.7 s and +40.2 s, and at +40.2 s the viewer read "Connection lost — reconnecting (attempt 1)", consistent with the Mac's every-few-seconds drop that #1856 fixed later that day (in 0.4.119); the census found four 2 px strips (`1512×2` top & bottom, `2×982` left & right) during the session and **zero** Roomler windows 60 s after it ended — the "gone after" the stream cannot show |
| the next click into the person's window acts | after a fresh banner reveal, one click into TextEdit placed the cursor and the typed "XY" appeared (then removed with two Backspaces). The banner did not steal focus |

*Corrected 2026-10-10, after re-reading this run's frames.* As first written, this
log called +40.2 s the teardown and said the banner was "hidden whenever the
pointer was elsewhere". The frames say otherwise. The stream carried one frame
between +32.7 s and +40.2 s, and the session was lost at +40.2 s. So the scene's
last check never reached the
Mac: hover the banner (+27.8 s), leave it (+33.1 s), expect a hide. In the last
frames the controller's pointer is far away, but the Mac's own cursor is still on
the banner. Every clause above still stands on the evidence it cites. A hide after
the pointer has hovered the banner is **unobserved**.

**"Being viewed by «name»" + a working Disconnect (macOS).** The banner reads
"Being viewed by «owner name» · keyboard + mouse · «org» · «elapsed»". Clicking
its Disconnect — in the stream, which maps to the real banner window on the Mac
— logged, on the device:

```
viewee requested disconnect via overlay badge session_id=…
PC state change session=… state=Closed
session terminated by server session_id=… reason=ControllerHangup
```

⚠️ The banner window is not key, so the FIRST click only activates the companion
(the menu bar went from TextEdit to Roomler) and a following click fired the
button — the webview has no `accept_first_mouse`. **Filed as #1852 and fixed in
#1854** (agent 0.4.118, carried by 0.4.119); the GREEN re-run is the
2026-10-08 (#1852) log at the end.

**macOS "Check for updates".** The companion's button runs `<daemon> self-update
--check-only` (`cmd_check_update`, via `agent_exe_path`, which on macOS resolves
to the installed daemon). Replicated as the console user: `Up to date (current:
0.4.117, latest: agent-v0.4.117)`, exit 0. "Apply update" has nothing to apply
on the latest release.

Everything restored: the test recording deleted, the uploaded census script
removed; the device's remote-recording gate was left ON as found.

### 2026-10-08 — #1852: the banner's buttons take their first click (0.4.117 RED → 0.4.119 GREEN)

The two-click nit the P9 run above found — the banner is shown without focus
(P9, `orderFrontRegardless`), so its window is not key and an inactive macOS
window spends its first click on activation unless its view accepts first mouse.
**#1854** adds `.accept_first_mouse(…)` to the banner's window builder
(`panels.rs`), derived from the show-without-focus path so it cannot drift; the
consent prompt is shown focused and keeps the default (a click-through on an
approval surface is not wanted). Shipped in agent **0.4.118** and carried by
**0.4.119** (the fix commit is an ancestor of both tags; the Mac took 0.4.119,
which also carries #1858 "remote desktop to a Mac no longer drops every few
seconds").

| check | 0.4.117 (RED) | 0.4.119 (GREEN) |
|---|---|---|
| one click on Disconnect | the first click only activated the companion (menu bar TextEdit → Roomler); the session kept running (`31s`), and a following click produced the `viewee requested disconnect` line (the P9 run above needed a multi-click sequence) | ONE click → `viewee requested disconnect via overlay badge` → `session terminated … ControllerHangup`, then the viewer auto-reconnected to a fresh session. Seen twice: 0.4.118 at 14:18:42Z and **0.4.119 at 14:38:43Z** (click and log line simultaneous within the +11.1 s Mac↔box clock skew) |

The 0.4.119 Disconnect run re-confirmed the P9 clauses on the shipped build
(frame detector over the banner box and the four screen-edge strips): banner up
at +1.7 s **without taking focus**, down at +4.6 s, back up at +6.8 s on a
top-edge rest, and the **red frame on every frame until the disconnect** at
+11.3 s, then gone. The fix is additive and does not touch the reveal watch, and
P9's next-click-acts and stays-while-recording clauses (verified on 0.4.117
earlier this day) are unaffected.

⚠️ **One-click Stop recording is the SAME banner window** — both buttons live in
its one webview, and `accept_first_mouse` is a per-window attribute, so the
Disconnect run proves the window now takes its first click for Stop recording
too. A **direct** one-click Stop-recording run on the Mac is still owed: two
attempts were lost, the first to the pre-#1858 every-few-seconds RC drop while
the Mac was mid-update, the second to the device owner's own live session on the
Mac. That demonstration is tracked here. #1852 itself had already closed when
#1854 merged (10:55Z, "Fixes #1852"), before this entry was written; it said
"stays open" until the 2026-10-10 correction.

### 2026-10-09 — P10: the host's Disconnect must end the session for good (RED)

**The requirement** came from the operator on 2026-10-09: a Disconnect clicked on the
viewed desktop has to stop the reconnect mechanism and tell the viewer they were
disconnected.

**RED — the current deploy.** The measurement is the #1852 GREEN run above, read for
what it ALSO showed: agent **0.4.119**, the Mac, **2026-10-08 14:38:43Z**, one click on
the banner's Disconnect → `viewee requested disconnect via overlay badge` →
`session terminated … reason=ControllerHangup` → the harness viewer auto-reconnected
to a fresh session within ~3 s (and the same on 0.4.118 at 14:18:42Z). The hosted image
serving the viewer then was `hosted-20261005-d2dc0ef` (promoted 2026-10-05 22:41Z);
re-checked on 2026-10-09 against the deploy repo's `newTag`: **unchanged, nothing
promoted since**, so that run was taken on exactly today's deploy and is the RED for
P10. The audit's `ControllerHangup` is the viewer's own hangup from its reconnect
ladder, sent when the control data-channel closed under it — the ordering explained
in the P10 design above.

### 2026-10-09 — P10 GREEN on the Mac's banner (0.4.121 + `hosted-20261009-cac4761`)

**What ran.** Agent **0.4.121** on the Mac, which its update helper installed at
02:36:17Z. The server and the viewer were on `hosted-20261009-cac4761`, promoted at
07:42Z, with both pods on it and 0 restarts. A Playwright field spec drove the live site
from the demo harness's saved session:
1. connect, full screen, and a top-edge move to reveal the banner;
2. **one** click on the banner's Disconnect, then a 65 s hold;
3. read the session audit, then press Reconnect;
4. cut the viewer's signalling socket and keep hands off.

The screen was lock-checked before every input. It ran twice. The browser host's clock
runs about 10 s behind the Mac's, so each side's times below are its own clock.

| check | run 2, 07:58Z (run 1, 07:52Z, agreed on every row it reached) |
|---|---|
| one click → the viewer is told | `rc:terminate reason=host_disconnect` reached the viewer **72 ms** after the click (run 1: 86 ms). The viewer logged `session ended by the person at the device - not reconnecting`. It left full screen by itself, because the full-screen target exists only while connected, and showed "The person at the device ended the session." with Reconnect |
| stays disconnected ≥ 60 s | 65.0 s after the click: **0** `rc:session.request` sent, the notice still shown, not connected. The agent log created no session in that window |
| the device's side | `viewee requested disconnect via overlay badge`, then 0.2 ms later `host disconnect: told the server; the peer closes after the grace reason="host_disconnect" grace_ms=1000`, then `session terminated by server … reason=HostDisconnect` (the hub's echo, +43 ms), then `PC state change … Closed` **1.003 s** after the click. The grace held the transport for exactly its length |
| the audit | the session's audit (`GET …/session/{id}/audit`) ends with `host_disconnect` |
| Reconnect is an explicit act | one click on the notice's Reconnect sent a new `rc:session.request` and connected in **2.4 s** (run 1: 3.5 s; consent is auto-granted to the owner by the device's policy). The notice was gone |
| an ordinary drop, after all of the above, still auto-reconnects | the viewer's `/ws` was closed from the page. The viewer logged `control channel closed mid-session — re-creating session`; the socket came back in 3.1 s (`signalling restored — retrying immediately`); a new session was up **6.3 s** after the cut, with **0 clicks** and no notice |

**Wrong turn, recorded.** Run 1's ordinary drop was the `context.setOffline(true)` for
10 s planned here, and it dropped **nothing**. The open `/ws` stayed open with no close
event, the WebRTC path is outside DevTools network emulation, and the session kept
streaming through the whole window. The check timed out on a session that had never
ended. A viewer whose network blips for 10 s keeping its session is correct, but it is
not the check. Run 2 cut the signalling socket itself, the chain a real blip triggers
when it kills TCP: the hub ends the session, the device closes the peer, and the
viewer's ladder brings it back.

**Side findings** (neither blocks the box):
- The notice's second line was dark grey on the black stage in the light theme, because
  Vuetify's `text-medium-emphasis` is theme-relative and `!important`. The stage's
  offline, reconnecting and no-media lines had the same problem. Fixed in the PR that
  records this run (`.remote-stage .text-medium-emphasis` stays light).
- An admin sees **Force control (admin break-glass)** next to "The person at the device
  ended the session." Break-glass predates P10 and is audited with a reason. Whether to
  offer it right after the person's own Disconnect was an operator decision.
  **Decided 2026-10-10: not offered.** The notice keeps Reconnect alone, which asks the
  person again; break-glass stays reachable from a fresh page. See
  `docs/remote-control.md` §11.5.

**Not covered:**
- **The Windows badge.** It was capture-excluded in this run; see the next entry.
- **The Linux companion banner.** It uses the same banner code and the same daemon arm,
  but was not exercised separately.

### 2026-10-09 — P10 GREEN on the Windows badge (0.4.121 + `hosted-20261009-cac4761`)

**What ran.** A corporate Windows laptop with one 1920×1200 display, on agent
**0.4.121**, checked unlocked before every input. The viewer was the operator's own
Chrome, driven by Claude in Chrome, on the live site, from a background tab.

**Aiming the click.** The badge is excluded from capture, so the stream cannot show it,
and `roomler exec` runs in session 0, so it cannot enumerate the user's windows. The
click was aimed by the badge's fixed geometry instead (`agents/roomlerd/src/indicator/win.rs`):
- the badge is 300×48, top-centre, at y = 4;
- its Disconnect is at client (196–292, 9–39);
- the agent is per-monitor DPI-aware, so that is physical (1006–1102, 13–43), centre
  (1054, 28), and the same point still lands inside the button if the badge were drawn
  DPI-unaware at 125 % or 150 %.

The input went through the viewer's own input path: pointer events dispatched on the
stage, which maps them to normalised coordinates and sends them over the input
channel. The stage's `setPointerCapture` was shimmed for the synthetic pointer id,
because the handler calls it before it sends the button-down. On the device it is
ordinary injected input, so the badge got a real `WM_LBUTTONUP`. The sequence was a
move to the top edge (the badge reveals after 1.2 s there), a move onto the button,
and **one** click.

| check | evidence |
|---|---|
| one click → the device's word, first | the agent: `viewee requested disconnect via overlay badge` 09:38:04.639Z, then `host disconnect: told the server … reason="host_disconnect" grace_ms=1000` **+1.0 ms**, then the hub's echo `reason=HostDisconnect` +30 ms, then `PC state change … Closed` **+1.031 s** (the grace) |
| the viewer is told | `rc:terminate reason=host_disconnect`; the notice "The person at the device ended the session."; the phase chip `closed`; **nothing** sent by the viewer after the click |
| stays disconnected ≥ 60 s | **67 s**: 0 `rc:session.request`, 0 outgoing frames of any kind, the notice still shown. The agent's last peer state change in the window is that session's `Closed` |
| the audit | `session_requested → consent_prompted → consent_granted → session_started → session_ended: host_disconnect` |
| Reconnect is an explicit act | one click: `rc:session.request` +39 ms, a **new** session, connected in **1.37 s**, the notice gone |
| an ordinary drop, after all of the above | the app's own signalling socket was killed mid-session (`forceRedial()` on its ws store, which closes the live socket and dials a new one). The viewer re-created the session by itself: `rc:terminate controller_hangup` for the dead one, a new request, connected **0.95 s** after the cut, no click, no notice |

The run ended with the viewer's own Disconnect (`controller_hangup` both ways), and the
tab was closed. The badge's Disconnect, the companion banner's and the ordinary-drop
ladder now all have field evidence. Only the Linux companion banner has not been
exercised separately (see the next entry).

### 2026-10-09 — P10 GREEN on the Linux companion banner, and a gap it found (#1911)

**What ran.** One vmtest cell, `ubuntu/installer/system@zeus --keep`: Ubuntu 24.04 with
a GNOME **Wayland** session, the release `.deb`s, a root daemon, agent **0.4.122**. Every
check passed: install, enroll, overlay, wayland, rd, the companion autostart and the
desktop views. Issue filing was off and the harness's `LATEST` verdict was restored after.
The viewer was a Playwright spec in the harness's own container on mars, signed in the way
`vmtest-remote.spec.ts` is. The guest was destroyed after the run.

**The gap, measured first.** The companion read *"Service offline"*. A root daemon's
LocalAPI is `/var/run/roomler/roomler.sock`, with the directory at 0700 and the socket at
0600, both root-owned. As the desktop user, `roomler status` fails with *"Permission
denied (os error 13)"*. So on a Linux **system** install the companion cannot see sessions,
and the person at the device gets no banner and no Disconnect. On Wayland, with no native
overlay, there is no companion consent prompt either. This predates P10 and is filed as
**#1911** (bug), with the options left to the operator. The root-only ACL is the trust
boundary, so widening it is not the fix.

**The banner, with the companion able to reach its daemon.** That is what a per-user
install has. On the throwaway guest only, the socket was opened (directory 0711, socket
0666), and the companion read **"Connected"** at once. The banner appeared at session
start: *"Being viewed by vmtest admin · keyboard + mouse"* with Disconnect, centred by
mutter. On Wayland it stays up, because the pointer cannot be read
(`panels.rs` `Sample::read`). It is in the stream, since Linux has no capture exclusion.
The click went to where the page showed the button.

| check | evidence |
|---|---|
| one click → the device's word, first | the daemon: `viewee requested disconnect via overlay badge` 21:43:23.0792Z, then `host disconnect: told the server … reason="host_disconnect" grace_ms=1000` **+0.06 ms**, then the hub's echo `reason=HostDisconnect` +1.6 ms, then `PC state change … Closed` **+1.002 s** |
| the viewer is told | `rc:terminate reason=host_disconnect` **33 ms** after the click; the viewer logged `session ended by the person at the device - not reconnecting`; the notice shown, its second line readable on the stage (#1899) |
| stays disconnected ≥ 60 s | **65.8 s**: 0 `rc:session.request`, not connected; no session on the daemon in the window |
| the audit | ends with `host_disconnect` |
| Reconnect is an explicit act | a new session in **544 ms**, the notice gone |
| an ordinary drop, after all of the above | the viewer's `/ws` was cut, and the viewer logged `control channel closed mid-session — re-creating session`. A new session came up **4.0 s** later, with no click and no notice |

All three P10 surfaces now have field evidence: the macOS banner, the Windows badge, and
the Linux banner where it can reach its daemon. On a Linux system install it cannot, and
that is #1911.

### 2026-10-10 — P11 on a Linux system install: RED on 0.4.123, GREEN on the 0.4.124 candidate

The setup was one vmtest cell: Ubuntu 24.04, GNOME on Wayland, the installer `.deb`s, a **system** (root) daemon, kept with `--keep`.
- **First, RED:** the released **0.4.123**.
- **Then, GREEN:** the **0.4.124 candidate**, installed over it on the same guest. It came from rehearsal run 38008544269 of the bump branch, unsigned, with the `.deb`'s SHA-256 checked against its sidecar.
- **The daemon's socket was never touched.**
- **The companion stayed on the RELEASED 0.4.123 throughout.** P11 needs no companion change, since the companion already tries the person's own `$XDG_RUNTIME_DIR/roomler.sock` first. This run is the proof.

| check | 0.4.123 (RED) | 0.4.124 candidate (GREEN) |
|---|---|---|
| the person socket | none | `/run/user/1000/roomler.sock`, `srw-------`, the person's (`vmtest:vmtest`) |
| the daemon's own socket | `/var/run/roomler` `drwx------ root:root`, socket `srw------- root:root` | unchanged |
| `roomler status` as the person | `Permission denied (os error 13)` | answers: the device, `version 0.4.124`, `mode service (SYSTEM)` |
| `roomler peers`, and `roomler config set auto_update false`, as the person | `Permission denied (os error 13)` | refused with the pointer "… Everything else goes to the daemon's own socket — run `sudo roomler …`"; the config line is unchanged |
| the companion, in the stream of a live session | "● Service offline", "Not enrolled yet" and "The Roomler service is not running on this computer right now" | "● Connected", naming the enrolled device |
| the banner during a session | none | *"Being viewed by vmtest admin · keyboard + mouse"*, with Disconnect (mutter centres it, as on 10-09) |
| one click on its Disconnect | — | see below |
| the person logs out | — | see below |

**One click on Disconnect (GREEN):**
- `rc:terminate reason=host_disconnect` arrived **32 ms** after the click.
- The notice showed with **no break-glass button** (#1917, live since the 00:32Z promote).
- For the next **65.8 s** there were no `rc:session.request` messages.
- The audit ends with `host_disconnect`.
- Reconnect connected in **566 ms**.
- An ordinary drop after that auto-reconnected in **4.0 s** with no click.

**The person logs out (GREEN):**
- The greeter (`gdm`, class greeter) took the seat.
- **18 s** later the daemon logged `person socket: nobody at the seat now; closing uid=1000`, and the socket was gone.
- `/run/user/1000` itself survived, because an SSH session held it. So the daemon removed the socket, not logind.
- There was **no socket** in the greeter's `/run/user/124`.

Not field-tested: root at the seat, since GDM on this image refuses a root login. `step()` ignores uid 0 by construction, locked by `the_socket_follows_the_person_at_the_seat`.

Found: the socket's own lifecycle lines never reach the log.
- **The lines exist.** `localapi: person-at-the-device socket up` / `… down` are emitted at info by the `roomler_localapi` crate.
- **The filter drops them.** That target is below every default filter: `roomlerd=info,tunnel_core=info,warn` in the generated units, and the binary's own default adds only the daemon's pre-FR-21 target. Only roomlerd's `person socket: …` lines (a move, a stop, a failure) are seen.
- **The result.** `ls`, not `roomler logs`, answers "is the person socket up?".
- **Why it is a follow-up.** Units already on disk set `RUST_LOG` wholesale, so a fix there needs the care FR-21's rename of the daemon's own target did (`logging.rs`, `mirror_legacy_log_target`).

The guest was destroyed by name afterwards.

### 2026-10-23 — `prompt_owner` and `push` on GNOME Wayland (agent and API 0.4.158)

The setup:
- **The device:** the lab's Fedora Asahi box (Fedora 42, aarch64, GNOME on Wayland), with a **system** daemon and the companion, both 0.4.158. The daemon had started the companion.
- **The server:** `hosted-20261023-978e47b` (0.4.158).
- **The controller:** Chrome on a Windows PC, signed in as the device's **owner**. That is what made `prompt_owner` testable at all; AC1's old note said every device was owned by an account other than the test one.
- **The policy:** written through the API (`PUT /api/tenant/{tid}/agent/{id}` with `access_policy`), not the dialog. Its as-found value, every field unset, was read first. After each mode it was put back and read back equal.

| run | access policy | the device's log | the device's screen | the controller |
|---|---|---|---|---|
| control | as found | `decision=Granted effective_mode=AutoGrant` | nothing | connected; first frame 2.3 s |
| prompt | `prompt_owner = true` (mode unset, so `prompt`) | `consent prompt surface … native=false notified=false have_surface=true`, then `operator consent required` | the companion's panel over its Devices window: *Remote control request*, *«owner» is requesting to control this device.*, *On behalf of «org»*, the four permissions, *Expires in 4:44*, Deny / Approve | *awaiting_consent*: "Waiting for the agent to allow the connection…" |
| Approve, 34 s later | same | `operator consent decision outcome=Granted` → `effective_mode=Prompt { timeout: 300s } granted=true` → `PC state … Connected` 1.5 s later | the banner *Being viewed by «owner»*, with Disconnect | connected; first frame 35.8 s, 34.0 s of it the wait |
| prompt, the companion's window hidden (Super+H) | same | the same three lines | the panel as **a window of its own**, centred, on top of a maximized app | awaiting_consent |
| Deny | same | `outcome=Denied` → `granted=false` → `session terminated … reason=UserDenied` | the panel gone | "Someone at that device declined the request." |
| restored | as found | `effective_mode=AutoGrant` | nothing | connected; first frame 2.7 s |
| push | `consent_mode = push`, `prompt_owner = true` | `owner-side consent (email/push) — agent waits for the server to resolve … consent_mode=Some(Push)` | nothing, by design | awaiting_consent until the owner answered |

**How `push` reached the owner:**
- The owner's in-app notification (*Remote control request*, *«owner» wants to control «device»*) was written in the same second as the request, and the bell showed its badge.
- Its `/consent/<token>` page took the Approve: *Approved — the session can now start.* The controller connected; first frame 35.6 s, 33.6 s of it the wait.
- **No web-push card exists to measure.** `GET /api/push/config` answers `vapid_public_key: ""`, and the server logged `Push consent mode but no push service is configured — owner cannot approve` (`crates/modules/fleet/src/consent_consumer.rs:254`).
- The in-app leg that every owner-side mode gets (`consent_consumer.rs:192`) carried the request. So "cannot approve" is wrong whenever the owner has the app open.

**What this establishes, and what it does not:**
- On GNOME Wayland, a root daemon has no native panel (`native=false`), as designed, and the companion served every prompt.
- That held with the companion's window hidden too: the panel opens a window of its own, and mutter put it on top.
- Not exercised here:
  - a companion that is NOT running (the daemon starts one first, `companion::ensure_running`);
  - a locked GNOME screen (a Linux host does not report `host_locked` yet; `docs/remote-control.md`, *A locked host*);
  - the notification fallback (FR-93 P6f).
- `email` was not run: it would mail the owner's real inbox.

**Found:**
- **The connect toast blamed a machine step for the owner's approval.** After the push run it read *"Connected in 35.6 s - slower than usual. Most of the wait was opening a network path to the device (1.5 s)."* The machine's share was 2.0 s; the rest was the owner approving. The prompt run's record has the same shape. Filed as #2787, fixed by #2788.
- **The `/consent/<token>` page names neither the controller nor the device** ("Someone is requesting…"), while the notification that links to it names both. Plausibly deliberate: the link is a bearer capability, and it is the same page an email carries. Recorded, not filed.

**A wrong turn:** the toast's total was first read as 23.0 s, off a half-scale screenshot. The browser's persisted record (FR-22 part 4: `agent_logs`, target `rc.connect`) says 35.6 s. Read the record, not the screenshot.

### 2026-10-23/24 — `email`, `prompt_then_email`'s owner half, and two concurrent viewers

The same box and controller as the entry above.
- **API:** `hosted-20261023-0c0d5ff` on 10-23 and `hosted-20261023-c680ec9` on 10-24.
- **Agent:** 0.4.158 on 10-23 and 0.4.162 on 10-24.
- **Policy:** set through the API as before. After each run it was put back and read back equal.

**`email`:**

| run | the device's log | the server | the controller |
|---|---|---|---|
| 10-23 21:18Z | `owner-side consent (email/push) — agent waits for the server to resolve` | `Email sent via SendGrid … subject="Approve remote control of «device»?"`, 0.2 s after the request | after 5 min: *"Nobody answered the prompt on that device in time…"* (`consent_timeout`) |
| 10-24 08:06Z | the same | the same; the owner confirmed both mails arrived | the owner opened the mail's link at 08:11:11Z and approved at 08:11:12Z, and the controller connected |

How the 10-24 approval is known to come from the mail: the front proxy logged a `GET /consent/<token>` with **no referrer**, which is a page opened from the mail rather than from the app, then `POST …/approve` 200.

The timeout's wording is wrong for an owner-side mode, because nobody at the device was asked. Filed as #2810.

**`prompt_then_email`, the owner half (10-23 21:24Z).** The two halves start together; it is "and", not "then":
- The companion's panel came up on the box ("Expires in 16s" at the screenshot).
- SendGrid sent the owner's mail 0.4 s after the request.

Nobody answered the panel. At 30 s the device logged `operator consent decision outcome=Timeout`, then `decision=Timeout effective_mode=Prompt { timeout: 30s } granted=false reason="timeout"`. The controller stayed on *awaiting_consent*: the session handed over to the owner, it did not end. The owner's Approve on `/consent/<token>` connected it, about 58 s after the request. No snackbar showed (#2787's fix, live since 20:39Z).

**Two concurrent viewers (10-24 07:55–08:03Z, agent 0.4.162).** The viewers were Edge (A) and Chrome (B) on the same Windows PC, both signed in as the owner.

| step | the viewers | the device's `input arbiter:` log |
|---|---|---|
| both connect | B: "2 viewers on this device (input: free)", frames decoding | `session registered … mode="free" sessions=2` |
| B switches to exclusive | both: "(input: exclusive)"; B holds the floor | `mode changed session=<B> mode="exclusive"` |
| A requests while B is idle | A takes the floor (`IDLE_TAKEOVER` is 2 s) | `floor request session=<A> granted=true` |
| B requests while A is active | B: WAITING…; A: "«owner» wants control · GRANT · IGNORE" | `floor request session=<B> granted=false` |
| A grants | A: REQUEST CONTROL | `floor grant holder=<A> to=<B> granted=true` |
| B, the holder, leaves | A: "1 viewers (input: exclusive)" with no REQUEST CONTROL, so A holds the floor | `session terminated … reason=ControllerHangup` |
| A switches back | "(input: free)" | `mode changed session=<A> mode="free"` |

A browser switch takes longer than the 2 s takeover window, so the holder would have gone idle. To keep it ACTIVE, the holder's page dispatched synthetic pointer moves for 90 s. They moved the box's pointer within a small area and clicked nothing; the agent's `ghost cursor broadcast … total=300` counts them.

**Two things that looked like failures and were not:**
- **B's first request ended `agent_busy`** ("This device has reached its concurrent session limit"). A third viewer on the same network, neither test browser, had taken the device's second slot 4 s earlier; `rc_max_sessions` defaults to 2. The front proxy shows B's page loading only after that session's request. The cap worked as built.
- **One B session ended `ControllerHangup`** about 35 s after its window went behind A's. A deliberate repeat did NOT reproduce it: one viewer, its tab hidden behind another tab for 38 s, and the agent's pump kept sending while the page stayed connected. The cause is unknown; recorded, not filed.

The box's access policy was restored and read back equal after every run.

### 2026-10-24 — the `exec` and `ssh` prompts on GNOME Wayland (agent 0.4.162)

The same box (`native=false`: the companion is the surface). The operator approved the run in chat. Every gate was opened for it and closed after:
- **Gate 1, the org's switches:** already on for this org (`remote_exec_enabled`, `remote_ssh_enabled`); untouched.
- **Gate 2, the caller:** the controller already holds `EXEC_DEVICE` and `SSH_DEVICE`.
- **Gate 3, the device's policies:** set through the API.
  - exec: `mode: on`, `consent_mode: prompt`, `run_as: console_user`, so the command runs as the person at the screen, not root.
  - SSH: `mode: on`, `account_mode: console_user`, `consent_mode: prompt`.
- **Gate 4, the device's own switches:** `exec_enabled` and `ssh_enabled` set to true. `ssh_enabled` takes effect only on a restart, so the daemon was restarted.

| request | what the companion showed | answer | result |
|---|---|---|---|
| `roomler exec … -- 'id -un; uname -sr'` | *Command execution request*: "«owner» (via «controller host») wants to run a command on this device", the command in a box, "Expires in 26s" | none in time | the CLI: "the operator at this device did not approve the command (Timeout)" |
| the same, again | the same | Approve at ~7 s | `wlab`, the kernel version, exit 0 |
| `roomler ssh … -- 'id -un; uname -sr'` | *SSH session request*: "ssh:«owner» (via «controller host»)@«overlay ip»:«port» wants to open an SSH session on this device", "run a command" | Approve at ~8 s | the CLI: "waiting up to 30s for approval at the device…", then `wlab`, exit 0 |

**Restored** in reverse:
- both policies back to their defaults, which the API reads back as absent;
- `exec_enabled` and `ssh_enabled` back to `false` (as found), and the daemon restarted;
- `ssh_account_mode` and `ssh_max_privilege` were never set.

**Re-proven closed:** `roomler exec` answers "remote execution is not enabled on this device", and `roomler ssh` answers "this device does not accept SSH sessions (its policy is off)".

**Found:** #2815.
- Each request appeared **twice**: in the companion's prompt window AND as a modal in its main window. Both close on one answer. On 10-23 the main window was centred, so its modal sat exactly under the prompt window and nobody saw two.
- The exec prompt reads "On behalf of **primary**", where the RC prompt said the org's name.
- The SSH prompt's requester line is a raw `ssh:…@ip:port`.

The fix for all three, one PR across the companion, the agent and the server:
- **The companion asks once.** Its main window's modal renders only when the prompt window could not be created (`panels::consent_modal_needed`); it was meant as that fallback all along.
- **One org rule.** `signaling::prompt_org` serves screen control and exec, and `rc:rpc.exec` carries `tenant_name` from exec `authorize`'s tenant read. It never shows the primary's label.
- **The SSH prompt names the caller as the grant words it** (`prompt_name`). The technical identity stays in the logs and activity reports.

### 2026-10-24 — `push` with its web-push card (API `hosted-20261024-9867bb4`)

**Push went live on roomler.ai today, approved by the operator:**
- VAPID keys were sealed into `roomler2-secret` (deploy #47) and applied by a rollout restart at 10:49Z.
- That happened only after #2818 had made push an opt-in (#2817). Before it, the app asked every signed-in user for notification permission on page load.

**The opt-in:**
- A fresh load made no prompt and no subscribe call.
- The notification panel offered *Turn on*. The operator clicked **Allow** in the browser's own prompt.
- The subscription registered (`POST /api/push/subscribe` 200).

**A `push` request** (owner as controller, `prompt_owner`, the lab's Fedora Asahi box):
- **The server** logged `Push notification sent` to the browser's push endpoint.
- **The service worker** displayed *Remote control request — «owner» wants to control «device»*, linking to `/consent/<token>`, within seconds.
- **The controller** read "Waiting for the device's owner to approve the connection…" (#2810).

**Found, #2827:** the owner clicked the toast, and that **hung up the session it was asking about**.
- The device logged `session terminated … reason=ControllerHangup` at 13:19:06Z, one second before the approval (13:19:07Z, `POST …/approve` 200).
- The cause: the service worker's click handler navigated the first open roomler window, the waiting viewer itself, to the consent page.
- The fix (#2828) focuses a window already on that page and otherwise opens a new one, and never navigates an open window away.

The device's access policy was restored and read back equal.

### 2026-10-24 — the card's click, and the bell's (API `hosted-20261024-b78373e`)

**#2827 GREEN, 18:58Z.** The controller's own viewer waited on a `push` request to the same box.
- WNS accepted the push (`Push notification sent`) and never delivered it to the PC. Windows' `PushNotification-Platform/Operational` log has no `Raw Notification received` for it, while the 13:18Z push on the same channel had arrived in under a second. So the card was shown by the same service-worker registration, with the request's real link. A click on any of its cards runs the one `notificationclick` handler.
- The operator clicked the card. The front proxy logged `GET /consent/<token>` with **`Referer: https://roomler.ai/sw.js`**: that is `clients.openWindow()`, a new window. The viewer's URL never changed.
- The approval came from that page 2 s later. The device went `Connected` 0.26 s after it, with no hang-up.

**Found, #2841, 18:29Z:** the bell had #2827's shape.
- The operator was in a live remote session to a Windows laptop, viewing it from a Mac. The push for a request to another device was lost the same way, and the request reached them through the bell in that tab.
- `handleClick` ran `router.push(notif.link)`. The viewer tab went to `/consent/<token>`, and the laptop logged `session terminated … reason=ControllerHangup` 3 s after the click.
- The fix flags the pages that hold a live session (`meta.liveSession`: both remote viewers and the call). From those, `openInAppLink` opens a notification's link in a new tab. Anywhere else the bell navigates in place as before.

The device's access policy was restored and read back equal after each run.

### 2026-10-24/25 — #2841 and #2815 GREEN (API `hosted-20261024-46f3f84`, agent 0.4.168)

**#2841 GREEN, 22:17Z** (fix #2853). Tab A held a live viewer to the same box; tab B raised a `push` request for it.
- A click on that request in tab A's bell opened `/consent/<token>` in a **new tab**. The front proxy logged its load with **`Referer: …/agent/…/remote`**, i.e. `window.open` from the viewer.
- Tab A's URL never changed. It read `connected` 80 s later, and its session ran until my own Disconnect, 108 s after the click.
- Re-proven on the way: someone at the PC clicked the push toast for the same request, first in Edge and then in Chrome. Each opened the consent page in a window of its own (`Referer: …/sw.js`), so #2827's fix was seen live in both browsers.

**#2815 GREEN, 00:47Z** (fix #2855). The daemon and the companion package on the box were both 0.4.168. The gates were opened exactly as in the 10-24 RED run.
- The companion's main window was **open** and dragged aside, so nothing could hide under the prompt window. Each exec and SSH request came up **once**, in the prompt window. The main window showed its own page and no modal.
- The exec prompt read "On behalf of **Grox**": the org's display name, from `rc:rpc.exec.tenant_name`. The RED run read "primary".
- The SSH prompt read "«owner» (via «controller host») wants to open an SSH session on this device", with no `ssh:` and no `@ip:port`.
- The daemon log keeps both forms on purpose. The prompt's own lines (`consent prompt surface (ssh)`, `operator approved the session`) carry the readable name. The audit lines (`exec finished caller=ssh:…@…`) keep the technical identity.
- Both commands ran as the person at the screen (`wlab`, exit 0).

**Restored** in reverse:
- both policies back to their defaults, read back as absent;
- `exec_enabled` and `ssh_enabled` back to `false`, and the daemon restarted;
- re-proven closed: exec says "not enabled", SSH says "its policy is off".

### 2026-10-25 — the "Being viewed by" banner on a Linux X11 desktop (agent and companion 0.4.169)

**What ran.** A kept vmtest cell, `ubuntu/installer/system@zeus`: Ubuntu 24.04 with GNOME 46, the release `.deb`s (0.4.169), and a root daemon. Every lane check passed, including the companion's autostart. GDM was then switched to **Xorg**, giving an active `x11` user session on seat0. The companion autostarted in it and read "Connected" to its system daemon (#1911 has been fixed since 0.4.124). The viewer was the harness's lifecycle spec in the pinned Playwright container on mars. The screen was read with root screenshots and `xdotool`, both in the person's own X session.

| check | evidence |
|---|---|
| the banner shows | the daemon: `being viewed: the companion's banner says so` at session start. The window (360×76) sat at the top centre, (460,48) on a 1280×800 screen |
| X11 behaviour (P9) | the pointer is readable on X11, so the banner tucked away after its first 4 s. A rest at the top edge brought it back: *"● Being viewed by vmtest admin · keyboard + mouse · 37s"* with **Disconnect** |
| one click → the device's word, first | a real X11 click on its Disconnect. The daemon: `viewee requested disconnect via overlay badge` 11:13:25.7417Z, then `host disconnect: told the server … reason="host_disconnect" grace_ms=1000` **+0.17 ms**, then `session terminated … reason=HostDisconnect` +1.4 ms, then `Closed` after the grace |
| the viewer is told | `status: disconnected` |
| stays disconnected ≥ 60 s | **79 s**: 0 `incoming session request` on the daemon after the click, and every viewer poll after it read disconnected (153 of them, against 96 connected before) |

The cell's daemon was stopped first, so its ephemeral device unenrolled, then that cell alone was destroyed. With the macOS banner (10-08) and the Linux banner on Wayland (10-09), the companion's banner now has field evidence on every desktop it serves. The Windows half of the criterion is the native badge, not the companion. Its Disconnect is proven (10-09), and its text still needs a look at a Windows device's own screen.

### 2026-10-25 — the Windows badge, read from the screen itself (agent and companion 0.4.169)

**Why this way.** The native badge (`indicator/win.rs`) is capture-excluded by design (`WDA_EXCLUDEFROMCAPTURE`): no viewer, no in-OS screenshot API and no recording can show it. That is the point of it, and also why its text had never been seen. A VM's framebuffer, read by the **hypervisor** (`virsh screenshot`), is what a person at the device sees, and no in-OS capture flag reaches it. The pointer was moved as a real one is: QMP `input-send-event` on the VM's USB tablet.

**What ran.** A kept vmtest cell, `win11/installer/attended@zeus`, on the release MSI (0.4.169), with every lane check passing. The viewer was the harness's lifecycle spec in the pinned Playwright container on mars.

| check | evidence |
|---|---|
| hidden until asked | the framebuffer during the session showed the thin red frame round the screen, and no badge |
| the reveal (1.2 s at the top edge) | after a 1.8 s rest the framebuffer showed the badge at the top centre: a red **"VA"** initials chip, **"Being viewed by" / "vmtest admin"**, and a red **Disconnect** |
| one click → the device's word, first | a tablet move onto the button, then a real left button down and up. The device: `viewee requested disconnect via overlay badge` 11:31:26.6586Z, then `host disconnect: told the server … reason="host_disconnect" grace_ms=1000` **+0.38 ms**, then `session terminated … reason=HostDisconnect` +1.7 ms |
| the viewer is told | `status: disconnected` |
| stays disconnected ≥ 60 s | ~78 s: no session request after the click, and every viewer poll after it disconnected (151 of them, against 73 connected before) |
| afterwards | the framebuffer: no badge and no red frame |

The cell was shut down gracefully (the service stops and the ephemeral device unenrolls), then that cell alone was destroyed. With the macOS banner (10-08) and the Linux X11 banner (above), the criterion is met on all three platforms.

### 2026-10-25 — the companion-version caption on a real row (agent 0.4.170)

**What ran.** The lab's Fedora box, a root daemon on agent 0.4.170 with its companion package (`.rpm`, aarch64). The box's companion was put one release behind its daemon: the 0.4.169 `.rpm`, its `.sha256` checked, installed as a downgrade.

**Getting a skew that holds took two tries, and the first is a finding.**

| try | at the daemon's start | outcome |
|---|---|---|
| 1 | the opt-out marker `/etc/roomler/disable-auto-update`, `auto_update = true` | `auto-updater armed`. FR-117's sync moved the companion back before the first heartbeat (`.asc` verified, `companion: package installed tool="dnf"`, `… from=0.4.169 to=0.4.170 restarted=1`, 5 s after the start). On a system install the marker is not this daemon's opt-out: filed as **#2901** |
| 2 | `auto_update = false` | `auto-update disabled`, and the skew held |

| check | evidence |
|---|---|
| the device reports it | the agent row: `agent_version` 0.4.170, `companion_version` **0.4.169**, on the first heartbeat after the start |
| the grid shows it | the live Devices grid, the box's row: `v0.4.170 · desktop v0.4.169`, the caption in warning amber, and the tooltip *"roomler-desktop is on v0.4.169 while the daemon is on v0.4.170 — the companion updates separately …"*. The other 20 rows, with no skew, showed nothing |
| it clears | `auto_update` back to `true` (as found) and the marker removed (it was absent before). At the next start FR-117 repaired the skew in **2.5 s** (`from=0.4.169 to=0.4.170 restarted=1`). The agent row then read 0.4.170, and the grid carried no caption anywhere |

The lab lock was held throughout and released after.
