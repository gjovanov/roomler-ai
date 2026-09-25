# FR-84: The companion and the installer, finished — it starts after install, explains itself, and its pages hold still

**Issue:** [#1633](https://github.com/gjovanov/roomler-ai/issues/1633) · **Status:** **proposed 2026-09-25** — P0 (this spec) ·
**Related:** [#1631](https://github.com/gjovanov/roomler-ai/issues/1631) (web: RC view stuck on the previous device) ·
[#1632](https://github.com/gjovanov/roomler-ai/issues/1632) (a terminated session leaves the consent prompt up) ·
[#1035](https://github.com/gjovanov/roomler-ai/issues/1035) (a route port held by a companion the daemon spawned) ·
FR-11 (server-side grids) · FR-27 (consent surfaces) · FR-61 (vmtest)

## Goal

**Installing Roomler ends with the companion open in front of the person who installed it,
saying what the device can do and asking the one question that matters — turn on the private
network? — and every companion page shows something a person can read and act on.**

The operator's report (2026-09-25) listed nine items across `roomler-desktop`,
`roomler-setup` and the web app. The two web-side defects are one-PR bugs and are tracked
alone (#1631, #1632). This FR carries the rest, because they share one surface — the
companion and the LocalAPI it stands on — and one acceptance bar: a person who never read
the docs can install, understand and operate the device.

## The field evidence

Measured on the reporting host (Windows 11, perMachine install, daemon 0.4.101, worker in
user context, seven declared routes) and on production, 2026-09-25.

### The Routes page: the data holds still, the pipe does not

The page's "Declared routes" and "Live forwards" appear and disappear every few seconds. The
daemon's own answers do not change:

```
6 samples of `roomler route ls` + `roomler flows` over 10 s:
  7 routes, every one `active`, the same flow ids (fl-1 … fl-48) every time
24 concurrent `roomler route ls` (8 at a time): 24/24 rc=0
```

The named pipe does. Three back-to-back raw `CreateFileW` opens of `\\.\pipe\roomler` —
exactly what the Rust client does — repeated 60 times, 100 ms apart:

```
open1=ok       60
open2=err231   60      ← ERROR_PIPE_BUSY
open3=err231   59
open3=ok        1
```

⚠️ A .NET `NamedPipeClientStream.Connect(0)` probe of the same thing reported 90/90 OK: it
calls `WaitNamedPipe` first, and `0` there means *the server's default wait* (50 ms). A probe
that waits cannot see a gap the Rust client does not wait through.

### The setup download

```
curl -sI https://roomler.ai/api/setup/windows → roomler-setup-0.4.48-x86_64-pc-windows-msvc.zip
curl -sI https://roomler.ai/api/setup/linux   → roomler-setup-0.4.48-x86_64-unknown-linux-gnu.tar.gz
curl -sI https://roomler.ai/api/setup/macos   → 404
/api/setup/latest-release                     → exactly one release: setup-v0.4.48 (2026-09-02)
```

The agent lane is at `agent-v0.4.102`. The resolver is correct — it serves the newest
`setup-v*` that exists (`crates/modules/fleet/src/setup_release.rs:377-398`); nothing has
cut one since 0.4.48, which shipped without macOS (`skip_macos`, before Apple signing).

## The mechanism, in the code (master `d5db02c0f`)

| Surface | Where | What it does today |
|---|---|---|
| Routes poll | `agents/roomler-desktop/src/front/tunnels.js:285`, `app.js:166-168` | two 2 s intervals armed at load: device view (1 connection) + routes + flows (2 more) |
| swallowed errors | `agents/roomler-desktop/src/commands.rs:1007-1011`, `:1052-1057` | `Err(_) => Vec::new()` — any failure renders as "no routes" |
| empty paint | `tunnels.js:126-129`, `:176-179` | `paintRoutes([])` hides the table; `paintFlows([])` hides the card |
| pipe server | `crates/localapi/src/lib.rs:1735-1810` | ONE listening instance; the next is created only after `connect()` returns |
| pipe client | `crates/localapi/src/lib.rs:2062-2082` | retries `ERROR_PIPE_BUSY` once, after 50 ms |
| settings list | `crates/agent-core/src/config_surface.rs:42-820` | 150 `(key, kind, description)` tuples, no grouping; `restart_required` hard-coded `true` (`:21, :827, :841`) |
| live keys | `agents/roomler-desktop/src/front/settings.js:60-63` | a hand-kept `LIVE_KEYS` list the daemon does not know about |
| restart | — | nothing in the desktop or on the LocalAPI can restart the daemon |
| encoder caps | `agents/roomlerd/src/encode/caps.rs:37, 548-559` | a process OnceLock filled at the first `rc:agent.hello`; never on the LocalAPI |
| drop folder | `agents/roomlerd/src/files.rs:1966-2030` | SystemContext → the active user's Downloads; else the process owner's; Windows fallback `%PROGRAMDATA%\roomler\<segment>\uploads`; not configurable |
| devices | `agents/roomler-desktop/src/front/devices.js:58, 179` | LocalAPI `Peers` (netmap) every 2 s, `p.name` only, no `display_name`, orgs mixed |
| agent-token routes | `crates/modules/fleet/src/lib.rs:331-340` | crash / self-unenroll / logs; nothing lists devices; the web grid and mesh are `AuthUser` |
| launch after install | `scripts/install.ps1:283-305`; `agents/roomler-setup/src/orchestrator_agent.rs:501-610`; `agents/roomlerd/wix*/main.wxs` | copy the EXE, launch nothing, register nothing |
| first launch | `agents/roomler-desktop/src/main.rs:57-75`; `tauri.conf.json:26` | `--view=` handled only in the single-instance callback; the window starts hidden |
| companion spawn | `agents/roomlerd/src/companion.rs:689` | `std::process::Command` (inherits handles) on the user-session path; the SYSTEM path already passes `bInheritHandles = FALSE` |
| wizard release | `.github/workflows/release-setup.yml` | runs only on a `setup-v*` tag or a manual dispatch |
| wizard icon | `agents/roomler-setup/icons/` | 72-byte `.ico` / 67-byte `.png`, 1×1, since `a25d77b36` |
| overlay default | `agents/roomler-setup/src/front/index.html:120-131` | `#adv-overlay` unchecked for every role |

## Key design

Decisions taken with the operator on 2026-09-25: the desktop lists the devices **this
device's netmap already carries**, not the org inventory; the wizard releases in
**lockstep** with the agent; "Apply now" is a **daemon self-restart under a detected
supervisor**; the wizard icon is a white R on **green `#2E7D32`**.

### S1 — the wizard ships in lockstep

`release-agent.yml` gains `dispatch-setup-release` (after `release`, `if: is_release`,
skippable with the repo variable `SETUP_LOCKSTEP=false`): it tags `setup-v<V>` at the agent
commit — a `GITHUB_TOKEN` push triggers no workflow, by design — then dispatches
`release-setup.yml --ref setup-v<V>` with `publish_release=true` (dispatch is the documented
exception). Running after the agent release, it can never block or poison it; a failure
goes red with the manual command in the log. `release-setup.yml` pins `target_commitish` to
the built commit (softprops otherwise defaults it to `master`), drops `skip_macos`, asserts
the dispatched ref matches the version, and opens/closes a release-failure issue like the
agent lane. The newest `setup-v*` then always sits near the top of the 100-release page the
resolver caches, so the resolver needs no change.

### S2 — the wizard icon and the overlay default

`scripts/gen-tray-icons.py` gains `--target desktop|setup`; the setup set is the same R mark
on `#2E7D32` (`icon.png` 512 px, `icon.ico` 16–256 px). The hand-staged macOS `.app` gets
`CFBundleIconFile`. CI's icon guard extends to `agents/roomler-setup/icons/` (PNG ≥ 128 px,
ICO with ≥ 5 entries including 256 px), so a placeholder cannot ship again.

The overlay box follows the role until the user touches it: checked for `daemon-system` /
`daemon-machine`, unchecked for per-user roles (a hint explains that the TUN adapter needs a
service install). The orchestrator applies the same default when the option is absent.

### D1 — Routes hold still

1. **One connection per refresh.** `cmd_tunnels_view` reads `RouteList` then `Flows` on one
   LocalAPI connection and returns `{available, reason, routes, flows}`.
2. **Never paint an error as data.** A sequence guard drops stale responses; on failure the
   page keeps the last good data and shows *"live data unavailable since HH:MM:SS · N
   failures · reason"*; rows are keyed by id and patched in place, so a button never moves
   under the cursor. A desktop log records every refresh error.
3. **The pipe always has a spare.** `serve_windows_at` runs a pool of accept tasks (default
   4, `localapi_pipe_pool`, `1` = today's behaviour), each owning one listening instance.
   The client backs off `ERROR_PIPE_BUSY` on `[5, 10, 20, 40, 80, 160]` ms (≤ 315 ms).
4. **Edit a route.** `Request::RouteUpdate { route }` replaces a declared route in one step
   under the config write lock; an invalid replacement leaves the old route running.

### D2 — Settings, grouped

`KEYS` becomes `&[KeyMeta { key, kind, group, tier, live, description }]`, so the compiler
makes every key say where it belongs. Twelve groups (Access & consent · Private network ·
Network carriers & relays · Routing & adapter · Tunnels & SOCKS · Roomler SSH · Remote
desktop · Capture & input · Video encoding · Video rate & latency · Files · Device &
service) and three tiers; an **Essentials** section — `overlay_enabled`,
`auto_grant_session`, `exec_enabled`, `remote_config_enabled`, `ssh_enabled`,
`encoder_preference`, `files_dir`, `power_policy`, `auto_update` — opens by default and every
group below it starts collapsed. `ConfigEntry` gains additive `group`, `group_label`, `tier`
and `default`; `restart_required = !live`, and `live` is true only where the daemon really
applies a change at once. An older daemon without the fields gets today's flat list.

### D3 — Apply now

`Request::RestartDaemon { reason }` restarts the worker through its supervisor. A new
`supervision::detect()` recognises the Windows SCM host and the per-user Scheduled Task (a
hidden `run --supervisor scm|task` they pass), systemd (`INVOCATION_ID`), launchd
(`XPC_SERVICE_NAME = com.roomler.*`) and the FR-43 macOS supervisor — and **refuses** when
none is present, so an orphan `roomlerd run` is never taken offline. The restart goes
through the existing internal-shutdown path (`mark_clean_shutdown`), so it is not counted as
a crash and cannot trip a rollback; the exit code is `0` under SCM (already *respawn*) and a
new `RESTART_REQUESTED_EXIT_CODE = 9` elsewhere (launchd relaunches only a non-zero exit; 9
is outside systemd's `RestartPreventExitStatus=7 8`). Rate-limited to one per 30 s;
`local_restart_enabled = false` disables it. Remote configuration still never restarts a
daemon — this verb is LocalAPI-only.

As built (`agents/roomlerd/src/supervision.rs`, `localapi_state.rs::restart_daemon`), with
what building it added to the design above:

- **systemd is proven twice.** `INVOCATION_ID` is inherited by every shell under GNOME
  Terminal, so detection also requires this process's cgroup leaf to be one of OUR units, by
  exact name; and before accepting, the daemon reads the unit's *effective* policy back from
  systemd (`systemctl show`: `MainPID`, `Restart`, `Restart{Prevent,Force}ExitStatus`,
  `SuccessExitStatus`) — this pid must be the unit's main process and the policy must
  restart exit 9. Any failure to get that answer refuses.
- **The 30 s record is on disk** (`restart-request.json` beside the config the daemon loaded,
  never inside `config.toml`), written with fsync + rename BEFORE the answer; a restart that
  cannot be recorded is refused, because the record is the loop bound — it binds the
  relaunched process too.
- **The answer reaches the caller before the shutdown starts**: the verb only arms the
  restart; the LocalAPI connection loop calls `restart_commit` after writing
  `DaemonRestarting` (a 5 s fallback commits if that loop never gets there).
- **The exit happens after the runtime is gone**, exactly as an auto-update's exit tears it
  down; only the code differs. An OS stop (`systemctl stop`, `launchctl bootout`) racing an
  accepted restart wins — the process leaves with its ordinary code.
- **More refusals:** a screen recording in progress (a restart would end it — the same work
  the updater defers for), a restart already under way, and — FR-43 only — a worker younger
  than 30 s (its supervisor counts an earlier exit as a failed start and gives up after five).
- **The Scheduled Task's relaunch is the caller's, and a pid says when it is done.**
  `NodeStatus.pid` and `DaemonRestarting.pid` let a caller tell the relaunched daemon from the
  one that is leaving (the old process keeps answering for a moment after it says yes); with
  `restart_by: caller` the caller runs `roomlerd service start` whenever nothing answers,
  again every 3 s — the task's `IgnoreNew` drops a start that lands while the old instance is
  still exiting. One implementation (`roomler_localapi::wait_for_restart`) serves the CLI and
  the companion; the companion spawns `service start` with `CreateProcessW` and no inherited
  handles (#1035).
- **`local_restart_enabled` is live** (read from the file per request): an owner's OFF is in
  force for the very next request, and turning it back ON does not need the restart it
  enables.

### D4 — Overview: what this device can encode, and where received files go

`Request::EncoderCaps` returns the probed cells (`ready`), `not_probed` before the first
hello, or `unsupported` on a build without encoders — it reads the OnceLock and never starts
a probe. The Overview card draws codec × backend with chroma, struck-through denied cells,
and the probe time.

`files_dir` (live, Files group, essential) sets where dropped files land. `~` expands against
the **active** user at use time, so one machine config stays right for every user. It must be
absolute, creatable and writable, outside system directories — and when the writer is
SYSTEM/root, inside the active user's profile: the LocalAPI admits interactive non-admin
users, and without that rule one could make SYSTEM write remote-supplied files anywhere. The
rule holds at set time **and** at use time (`download_dir()` validates and falls back with a
warning). The card shows the effective path with Open folder, Change… (the native picker,
which can create a folder) and Use default.

### D5 — Devices: the mesh-visible grid and the mesh graph

- **Server** — `GET /api/agent/self/devices` and `/api/agent/self/mesh`, host-owned (the
  network module is optional), agent-token authenticated through the fleet extractor's
  shared `authenticate()`. The visible set is **self ∪ the join-time netmap shaping**,
  extracted as `shape_full_netmap` from `crates/modules/network/src/overlay.rs:527-550` and
  reused by `handle_overlay_join`, so the list can never show a peer the netmap withholds:
  `off`/`warn` list every live node, `enforce` only the shaped set. The device's own live node
  is found **before** any ACL read (`try_load_acl` creates the network on miss and must never
  run on a GET). Grid logic (`parse_query`/`apply_query`) is extracted from
  `crates/api/src/routes/device.rs` and shared by both listings. Rows carry display name,
  name, OS, version, presence, overlay IP, MagicDNS, tags, reachability and `is_self` —
  never `machine_id`, owner ids, keys, consent or codecs, and fields not returned are blanked
  before search. The mesh payload is the web one restricted to the visible set, edges only
  when both ends are visible, with a server-computed `label` and `self_node_id`.
- **Daemon** — `Request::Devices {org, page, per_page, q, sort, dir}` / `Request::Mesh {org}`
  proxied with the org's agent token; `Response::Upstream {code, message}` names the failure
  (server unreachable, unauthorized, old server, module unmounted, rate limited, unknown
  org); a short single-flight cache; `roomler devices` in the CLI.
- **Desktop** — a grid with debounced search, server sort and paging, a column chooser (show
  / hide, drag and ▲▼ reorder) persisted per org, keyed in-place rows, a live
  connection/RTT column merged from the local `Peers` poll, and a vanilla-SVG port of the web
  mesh graph (`ui/src/components/stats/MeshGraph.vue`) whose pure helpers are generated from
  `ui/src/utils/mesh.ts` with a drift test. The list refreshes every 30 s only while the view
  is visible. A new desktop on an old daemon or server falls back to today's peers table.

### D6 — the companion starts after install, and says hello

- **Launch once, from the daemon** — which covers every install path at once, since each of
  them ends with the daemon starting: only when `companion_autostart` is on, the companion
  exists and is not running, an interactive session exists, and no marker says it was
  already launched (the marker is written first). A companion the person quit is never
  relaunched; the consent path's `ensure_running` is unchanged.
- **No inherited handles** — the user-session spawn moves to `CreateProcessW` with
  `bInheritHandles = FALSE` (#1035 found tunnel ports held by a companion whose parent was a
  dead daemon), and a route stuck on `AddrInUse` logs who holds the port.
- **Autostart** — `HKLM\…\Run` for perMachine (written by the elevated `service install`,
  self-healed, removed on uninstall), `HKCU\…\Run` for perUser, xdg autostart on Linux,
  `RunAtLoad` on macOS; a "Start at login" toggle.
- **First run** — the companion reads argv on its first launch too, and shows the Welcome
  view until the person finishes it (per-user state), however it was launched: enrolled as
  `<name>` (or the enroll form) · the private network explained, with **Enable** → Apply now
  → the overlay IP (per-user installs get userspace mode on a free port, explained) · consent
  · where received files land · start at login · done.

### D7 — docs and the install matrix

`docs/desktop-companion.md` (new, indexed), updates to installation, tunnels,
remote-control, api, overlay-communication and deployment docs and the ship-it skill, and a
vmtest `companion-autostart` check on every install cell.

### Compatibility

Every LocalAPI and wire change is additive (`serde(default, skip_serializing_if)`); an
unknown verb is an error the desktop turns into "this service predates …", never a blank
page. Old desktop + new daemon, new desktop + old daemon, and new daemon + old server each
keep today's behaviour for the part the older side lacks.

## Phases

| phase | what | kill switch | status |
|---|---|---|---|
| P0 | spec + ledger row + issue | — | this PR |
| S1 | wizard lockstep + one-time `setup-v0.4.102` re-cut | repo variable `SETUP_LOCKSTEP=false` | — |
| S2 | wizard icon + overlay default for perMachine | revert (assets / UI) | — |
| D1 | Routes: one connection, stale-while-error, keyed rows, route edit; LocalAPI listener pool + backoff | `localapi_pipe_pool = 1` restores one instance | — |
| D2 | grouped Settings, per-key `live` truth | an old daemon → flat list | — |
| D3 | `RestartDaemon` + Apply now | `local_restart_enabled = false` | — |
| D4 | encoder-caps card; `files_dir` | `files_dir` unset = today's ladder | — |
| D5 | `/api/agent/self/*`, daemon verbs + `roomler devices`, grid + mesh | any error → the legacy peers table | — |
| D6 | launch once after install, autostart, Welcome, no-inherit spawn | `companion_autostart = false` | — |
| D7 | docs, vmtest check, field log | — | — |

New keys (`files_dir`, `local_restart_enabled`, `companion_autostart`, `localapi_pipe_pool`)
are registered in the config surface like every other key.

## Acceptance criteria

- [ ] **AC1** — An `agent-v0.4.N` release produces `setup-v0.4.N` at the same commit with no
      human step, and `/api/setup/{windows,linux,macos}` serve 0.4.N.
- [ ] **AC2** — The shipped wizard shows a white R on green at 16–256 px (EXE resource and
      `.app`); CI refuses a placeholder icon.
- [ ] **AC3** — The wizard's overlay box is checked for perMachine roles and unchecked for
      perUser roles, unless the user changed it.
- [ ] **AC4** — With the Routes view open for 10 minutes on the reporting host, no declared
      route or live forward disappears; the 3-open raw pipe probe returns no 231; a failed
      refresh keeps the last data and names the reason.
- [ ] **AC5** — Editing a live route's port leaves it `active` on the new port; an invalid
      edit leaves the old route running.
- [ ] **AC6** — Settings shows grouped, collapsible keys with Essentials open; search finds a
      key inside a collapsed group; a live key saves without a restart prompt.
- [ ] **AC7** — Apply now on a supervised Windows service returns the worker in < 10 s with
      `crash_count` unchanged; an unsupervised `roomlerd run` refuses; systemd and launchd
      hosts verified via `roomler exec`.
- [ ] **AC8** — The Overview caps card matches `roomlerd caps` on a Windows NVENC/QSV host and
      a Linux VAAPI host; a signalling-only build says "unsupported"; querying it never
      starts a probe.
- [ ] **AC9** — The Overview shows where dropped files land; changing it makes the next drop
      land there without a restart; a SYSTEM-context worker refuses a folder outside the
      active user's profile at set time and at use time.
- [ ] **AC10** — The desktop grid lists exactly this device's netmap set plus itself
      (integration test with a negative control under ACL `enforce`), shows display names,
      pages / sorts / searches on the server, keeps column order and visibility across
      restarts, draws the mesh for the same set, and no row carries `machine_id`, owner ids
      or keys.
- [ ] **AC11** — After install on Win11 (served script and wizard, perMachine and perUser),
      Ubuntu (`install.sh`) and macOS (`.pkg`), the Welcome view is up within 10 s of the
      daemon starting; a companion the user quit is not relaunched by a daemon restart;
      later logins start it in the tray.
- [ ] **AC12** — "Enable" in the Welcome view ends with an overlay IP shown (perMachine) or
      userspace mode on a free port with an explanation (perUser).
- [ ] **AC13** — A companion the daemon launched holds no handle of the daemon's process
      (handle listing after a daemon restart).
- [ ] **AC14** — Docs: `docs/desktop-companion.md` created in house style (page map, data
      sources, autostart per platform, the restart verb, `files_dir` rules, the devices
      visibility rule, mermaid diagrams) and linked from `docs/README.md`; the installation,
      tunnels, remote-control, api, overlay-communication and deployment docs and the
      ship-it skill updated.

## Open decisions

1. **Served-script parity for the overlay default.** `scripts/install.ps1` has no overlay
   option and the Linux `install.sh --system` path passes no `--overlay`. Keeping them as
   they are: flipping the default would silently change corporate automation. Revisit with
   an explicit `--no-overlay` if the operator wants parity.
2. **Tags in the device grid** — shown, as the same class of admin-authored label as
   `display_name`.
3. **Listener pool size** — 4; revisit with field data.

## Out of scope

- #1631 and #1632 — one-PR bugs, tracked on their own.
- `ConferenceView`'s param reuse — a remount there is a leave + join mid-call.
- `display_name` on the netmap wire (`NetmapPeer`) — the legacy fallback keeps netmap names.
- A per-agent rate ceiling on `/api/agent/self/*` beyond the per-IP limiter (the desktop
  polls only while the view is visible).

## Field-verification log

| date | build | observation |
|---|---|---|
| 2026-09-25 | agent 0.4.101 (reporting host) | **Baseline, Routes**: 6 samples of `route ls` + `flows` over 10 s — 7 routes `active`, identical flow ids; 24/24 concurrent `route ls` OK. Raw 3-open pipe burst ×60: open2 = `ERROR_PIPE_BUSY` 60/60, open3 59/60. A .NET `Connect(0)` probe of the same saw 90/90 OK — it waits (`WaitNamedPipe`) where the Rust client does not |
| 2026-09-25 | production | **Baseline, wizard**: `/api/setup/windows` and `/linux` → `0.4.48`; `/api/setup/macos` → 404; `/api/setup/latest-release` → one release, `setup-v0.4.48` (2026-09-02); agent lane at `agent-v0.4.102` |
