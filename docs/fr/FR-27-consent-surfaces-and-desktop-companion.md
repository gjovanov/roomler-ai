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
8. ~~**macOS "Check for updates" targets the wrong mechanism.**~~ **Wrong —
   corrected during implementation.** The reported
   `Spawning self-update: No such file or directory (os error 2)` made this
   look like a second, independent macOS defect. It is not: `self_update_cmd`
   has queued the root `com.roomler.update` helper for a non-root macOS caller
   since FR-5 (`main.rs:3251`), so the mechanism was already correct and
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
| 0 | FR + issue + ledger | n/a | **done** — the FR-24 collision was already repaired on master by #850 |
| 1 | Consent correctness — owner override, local floor, timeout≠deny, `prompt_then_email`, `.pending` for exec+ssh, `roomlerd consent ls` | `prompt_owner` defaults to today's behaviour; the floor only tightens | **implemented** — timeout≠deny and `prompt_then_email` field-verified on 0.4.16, `.pending` for exec+ssh on 0.4.18, **the local floor on 2026-09-25** (AC4, WSL node, 0.4.102); `prompt_owner` still needs an owner-account run |
| 2 | Desktop companion — one tray with an icon, daemon-path resolution, update-output surfacing, version honesty, `ensure_running()` | each item independent; no wire change | **implemented** (version honesty deferred to a follow-up) — field pending |
| 3 | `PromptSurface` — 3.0 selection layer, 3.1 native Windows consent panel, 3.2 Tauri companion panels, 3.3 native X11, 3.4 native macOS | per-backend cargo feature; probe failure falls back to the companion | **all implemented** — field pending. ⚠️ `viewer-indicator-macos` is compiled by CI but deliberately **NOT in the macOS release feature set**: it moves tokio off the main thread, i.e. changes how every Mac agent STARTS, and macOS updates are owned by the root helper — a daemon that fails to start cannot pull its own fix. Enable only after a dispatch artifact runs on a real Mac |
| 4 | Linux packaging — a **separate** `roomler-desktop` .deb | absent package = today's behaviour | **implemented** — field pending |
| 5 | Input arbitration — mode re-seed, visible floor requests, single-viewer rail, deterministic handover | none needed (bug fixes) | **implemented** — field pending |
| 6 | Field test on the GROX fleet | n/a | **partly done, 2026-08-29 on 0.4.16** — the Windows native panel is verified end to end and the field log below lists exactly what is not. The test itself found 3 defects (#877), one of which froze the whole pre-0.4.16 Linux fleet |
| 8 | Field fixes — release-asset ordering, the virtual-desktop guard, the CLI name, and phase 2d's `companion_version` | the ordering fix is server-only and additive; the x11 guard only ever DECLINES | **deployed** (#877; API `v20260829-0d5078f44e42`, agents ≥ 0.4.18) — the guard and the ordering were field-verified 2026-08-29; `companion_version` read on the live grid 2026-09-25: on the wire for all 21 devices, rendered only on a skew, and the fleet has none (AC10 half; see the log) |
| 7 | Docs — `docs/remote-control.md` §11.2, `CLAUDE.md` known-issues | n/a | **done** — §11.2 rewritten (76bd6ef6) and the 2026-04-17 known-issue replaced rather than deleted; the resolution diagram + the floor's field line added to §11.2 on 2026-09-25; the docs criterion is in the list below |

## Acceptance criteria

Ticked only where a run is recorded in the field log below.

- [x] With `consent_mode = prompt`, controlling a device puts a panel on that
      device's screen and Approve starts the session — **Windows**, 0.4.16.
      ⚠️ The `prompt_owner = true` half is untested: every fleet device is owned
      by a different user id than the test account, so the toggle never renders.
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
- [ ] All five modes exercised end-to-end on the fleet, each recorded with the
      surface that served it. `auto`, `prompt` and the host half of
      `prompt_then_email` are done; `email` / `push` resolve at the **owner**,
      a different account here.
- [ ] An `exec` prompt and an `ssh` prompt render on the same surface as an RC one.
      ⚠️ Blocked on `EXEC_DEVICE` / `SSH_DEVICE`, which are deliberately not in
      `DEFAULT_ADMIN` — an operator grant, not a code change.
- [ ] A live session shows "Being viewed by «name»" with a working Disconnect on
      Windows, macOS and a Linux X11 desktop. (Windows border confirmed visible +
      capture-excluded during a session; the reveal-on-hover badge and the other
      two OSes are untested.)
- [x] `roomler-desktop` shows exactly one tray icon, with a menu — **Windows**,
      measured 2 → 1 against the 0.4.15 build. macOS/Linux untested.
- [ ] macOS "Check for updates" and "Apply update" both work from the companion.
- [~] The Devices grid shows the companion version alongside the agent version.
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
- [ ] `free` ↔ `exclusive` verified with two concurrent viewers; the device policy
      re-applies after every session ends. **Second half done** (re-seed proven on
      one daemon process, no restart); the two-viewer half is blocked by a
      same-host media failure and needs a second controller machine.
- [x] The `roomlerd` .deb's `Depends` still contains no GTK/webkit entry —
      `libasound2, libc6, libxcb-randr0, libxcb-shm0, libxcb1`, and the guard that
      asserts it actually runs now (#871).
- [x] **Docs** — `docs/remote-control.md` §11.2 documents the consent model in
      the house style (the five modes and their windows, the owner shortcut, the
      device's floor, the surface chain, the verdict grace) with a mermaid
      diagram of how one session's consent is resolved, and is linked from
      `docs/README.md` (the docs-before-close rule). The field-result half of
      phase 6 lives in this spec's log.

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
(`crates/modules/remote/src/controller.rs:436`) resolves `auto` by the owner
shortcut; the device's server policy was additionally set to `consent_mode =
auto` for the run (as found: unset) so the directive did not rest on the
shortcut alone. Nothing in the consent code differs between the two agent
builds below (`git log agent-v0.4.101..agent-v0.4.102` touches no consent
file), and the hub sends `consent_timeout_secs = host_prompt_timeout_secs =
300` for `auto` and `prompt` alike (`crates/modules/fleet/src/hub.rs:888`,
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
`agents/roomlerd/src/signaling.rs:2302`), yet the prompt stood for **300 s** —
the server's window — which is exactly the shape `strictest_of`
(`agents/roomlerd/src/consent.rs:136`) produces when it downgrades an `auto`
directive: `Prompt { timeout: host_window }` (`signaling.rs:2352`). The control
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
(`ui/src/components/admin/AgentsSection.vue:345`); the companion half,
`· desktop v<companion_version>`, renders **only when the companion disagrees
with the daemon** (`companionSkew`, `AgentsSection.vue:354-358,1671`), and the
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
