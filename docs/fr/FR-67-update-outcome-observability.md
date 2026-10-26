# FR-67 — An update's outcome is written, and nothing ever reads it

**Issue:** [#1267](https://github.com/gjovanov/roomler-ai/issues/1267) · **Status:** in progress (P1, P2, P7 shipped; P1+P2 field-verified on 0.4.172 and re-read on 0.4.174, 2026-10-26; P3 built — its field read is owed on the release that carries it; P7 field-verified on 0.4.146; P5 open) · **Found:** field-testing `agent-v0.4.48`, 2026-09-02 · **Doc:** [`docs/update-outcome.md`](../update-outcome.md)

## Goal

Make "did this update succeed?" answerable — on the device, and across the fleet.
Today the question has an answer computed for it, and that answer is thrown away.

## The evidence

`roomlerd` already computes a verdict for every install: `InstallStatus` is a
five-state enum (`InProgress | SucceededVerified | SucceededUnverified |
InstallerFailed | Timeout`, `post_install.rs:131-151`) persisted to
`<log_dir>/last-install.json` (`post_install.rs:166`).

Measured on the fleet:

| host | record written | for | status |
|---|---|---|---|
| cluster node (Ubuntu 24.04) | **2026-08-29** | `agent-v0.4.16` | `InProgress` |
| aarch64 (Fedora/Asahi) | 2026-09-02 14:08:28 | `agent-v0.4.50` | `InProgress` |

The first row is the striking one: that host has since reached 0.4.50 — dozens of
updates, and not one recorded an outcome.

## Root cause — two independent defects, and the first one is not the cgroup

**1. The watcher waits for its own parent to die, and that death is what kills it.**

`install_tarball_linux` completes the entire install **synchronously** and returns
`Ok(std::process::id())` — *the daemon's own pid* (`updater.rs:2207`). That pid is
handed to the watcher as `--installer-pid`, and `watch()` calls
`wait_for_pid(installer_pid, INSTALLER_BUDGET)` unconditionally
(`post_install.rs:226`), polling `kill(pid, 0)` for up to 600 s
(`post_install.rs:1024-1049`).

So the watcher's only exit condition is the daemon's exit — while the daemon's
exit is precisely what tears down the cgroup and kills the watcher. There is
nothing being waited *for*: the install finished before the watcher existed. The
`.deb` path has the same shape — `run_linux_install_candidates` calls
`child.wait()` and returns an already-exited pid.

**2. The cgroup teardown then removes any chance of finishing.** Both packaged
units ship with no `KillMode` (`packaging/linux/roomlerd.service`,
`roomler.service`) ⇒ systemd's default `control-group`. `spawn_watcher` launches
the watcher as a plain child (`updater.rs:2577`) on the reasoning that it "is
reparented to init and runs to completion on its own" — but reparenting to PID 1
does not leave the cgroup. `docs/fr/FR-36-wayland-capture.md:371` already recorded
this exact lesson for a different subsystem.

⚠️ **The original diagnosis in #1239 named only (2).** (1) is the more important
half: with the wait removed, the watcher finishes in ~2 s and the cgroup race is
mostly moot. Fixing (2) alone would be building a systemd mechanism to support a
wait that should not exist.

**3. Nothing reads the verdict, on any platform.** `post_install::read_outcome()`
(`post_install.rs:180`) has **zero callers**. The module doc's claim that "the new
binary reads `last-install.json` at startup to surface the outcome"
(`post_install.rs:84-85`, echoed at `:10`, `:115`, `docs/remote-control.md:1256`)
was never wired. So even on Windows — where the staged watcher does produce a
verdict — nothing surfaces it. **P3 wires it** (2026-10-26): the daemon reads
the record at startup and on every `roomler status`, see the P3 section below.

**4. The server never learns.** No wire message carries an install outcome. The
server infers success only from a changed `agent_version` on the next hello, which
cannot distinguish *failed*, *rolled back*, and *hasn't polled yet*.

## Key design

**Stop waiting for an installer that has already finished.** That is the whole of
P1, and it needs no systemd, no new process supervision, and no new failure modes.
With the wait gone the watcher's life is: write `InProgress` → settle 2 s → run
`<origin-exe> --version` → write the terminal record.

⚠️ **This is a race, not a guarantee**, and the spec should say so rather than
claim a fix it does not deliver: if the daemon's teardown completes in under ~2 s,
the cgroup kill still lands mid-probe. **P6** closes that, and is deliberately
gated on measuring that the race is actually being lost.

### Why not go straight to a systemd transient unit

It was the first design, and it was refuted:

- **The safety premise is false as stated.** "Any `systemd-run` failure falls back
  to today's spawn" holds for `Command::spawn` failing, but `systemd-run` exits **0**
  once the start job is *enqueued* — an `execve` failure (ENOENT, an LSM denial)
  surfaces asynchronously as a failed unit, and `--collect` then garbage-collects
  it. The daemon would believe it launched a watcher that never ran. `--service-type=exec`
  is what makes the premise true, and it is therefore load-bearing, not a nicety.
- **It pays exit-path risk for a forensic-only payoff** (nothing gates on this
  file — see 3), and `.output()` with no timeout on the daemon's exit path is a
  genuine fleet-freeze mode.
- **The config kill switch is inert against that mode**: config changes are
  `restart_required` (`docs/remote-config.md:215-229`), and the failure being
  guarded blocks the very exit a restart needs.
- **It would create a failure mode that cannot exist today**: with the watcher
  surviving, PID reuse on the daemon's freed pid makes `kill(pid,0)` keep
  succeeding ⇒ a false `Timeout` on a healthy install. Removing the wait makes
  this unreachable.

### What must NOT change

- **Windows keeps its wait.** The MSI installer is genuinely asynchronous and the
  wedge-recovery path (`recover_wedged_install`) depends on it.
- The pid stays in the record for forensics; only the *wait* changes.

### P3 — the read half

Built 2026-10-26 (#2924), once the field read of that morning showed P1 and P2 holding
on the auto-update path (13 of 13 organic updates on one `.deb` host wrote
`SucceededVerified`, field log). The record now has three readers, none of
them SSH; the full treatment, with the seven words and the per-role log dirs,
is [`docs/update-outcome.md`](../update-outcome.md).

```mermaid
flowchart LR
    W[post-install watcher] -->|writes| F[last-install.json]
    F -->|read at startup| L["one `last install:` log line<br/>INFO · WARN by verdict"]
    F -->|read on every answer| S["NodeStatus.last_install<br/>→ `roomler status` · `--json`"]
    F -.->|P5, open| H["the hello's caps<br/>→ the fleet answer"]
```

| Piece | Where |
|---|---|
| The wire word, `LastInstallStatus` — the five verdicts spelled as the file spells them, plus `Absent` and `Unreadable`, and the 25 min line past which an `InProgress` never reported | `crates/localapi/src/lib.rs:465`; `NodeStatus.last_install`, `:439` |
| The read, fresh from disk every time | `post_install::last_install_status` (`agents/roomlerd/src/post_install.rs:1168`), its pure half `summarize` (`:1181`) |
| The startup line and its level | `log_at_startup` (`:1238`), `startup_level` (`:1222`); called right after `agent starting` (`agents/roomlerd/src/main.rs:3679`) |
| The status answer | `DaemonState::status` (`agents/roomlerd/src/localapi_state.rs:858`) |
| The `last update` line, one sentence per word | `last_install_line` (`agents/roomler-cli/src/localclient.rs:2819`), printed under `version` (`:2429`) |

- ⚠️ **Read on every answer, never cached at startup.** On Windows the MSI
  starts the new service while the watcher is still waiting on msiexec, so
  the verdict lands *after* the new daemon began (dev box, 0.4.172: daemon up
  at 1792967954, verdict at 1792967960). A startup snapshot would show
  `InProgress` for the daemon's whole life.
- ⚠️ **Three states, three words.** No key = a daemon that predates the field,
  and the CLI prints nothing. `Absent` = no record. `InProgress` = a record
  with no verdict, "still at work" while the watcher can be alive and `NEVER
  REPORTED` once it cannot. Collapsing any two recreates this bug one layer
  up; pinned by tests on both ends.
- ⚠️ **The reader runs where the updater runs.** On a Windows perMachine
  host that is the console-session worker, under whichever account the
  install mode gives it — the signed-in user on an attended install (the dev
  box, `privileged: false`), LocalSystem under SystemContext — so `<log_dir>`
  is that account's `%LOCALAPPDATA%…\logs`, where the record is; never the
  SCM host, whose `service-logs` dir is never written. Read on the attended
  mode; the SystemContext mode is unread (`docs/update-outcome.md` §5). A
  two-half Mac's per-user half reads its own dir — the root helper's installs
  are not its to claim — so it says `Absent` honestly.
- Windows' watcher is untouched: the diff adds readers and nothing inside
  `watch()`; the 10-26 record on the dev box shows the kept 23 s wait and the
  service settle (AC7).

### P7 — an update the device was never going to act on

Found 2026-10-19. The operator's **Update all** "updated" a two-org device three
times, and its journal logged `rc:agent.update ignored — only the primary
enrollment drives the machine-wide self-updater` three times. The dashboard's org
was the device's **secondary** enrollment. A device also drops the push when its
auto-update is off (#1689). Both refusals showed up only on the device (a WARN and
a LocalAPI counter). The frame had reached the socket, so the server counted it
`delivered`. That adds a fourth case to *failed*, *rolled back* and *hasn't polled
yet*, and no version check can tell it apart from them.

```mermaid
sequenceDiagram
    participant D as roomlerd (one enrollment)
    participant S as server
    participant UI as dashboard
    D->>S: rc:agent.hello, and every heartbeat re-announce<br/>update_mode = self | primary_org | off
    S->>S: store capabilities (replaced wholesale)
    UI->>S: GET agents
    S-->>UI: capabilities.update_mode
    Note over UI: "Update now" disabled with the reason,<br/>the Update-all dialog lists who will not update
    UI->>S: POST …/agent/update
    alt update_mode = primary_org or off
        S-->>UI: refused + refused_code (409 on one device), never sent
    else self, or unset (an older agent)
        S->>D: rc:agent.update
        S-->>UI: delivered
    end
```

| Piece | Where |
|---|---|
| The word, per connection | `AgentCaps::update_mode` (`crates/remote_control/src/models.rs:353`); `UpdateRefusal` and `update_refusal()` (`:562`, `:1416`) |
| Computed from the two facts the `UpdateNow` arm checks (`agents/roomlerd/src/signaling.rs:4879`) | `updater::update_mode` (`agents/roomlerd/src/updater.rs:3038`): `primary_org` on a secondary enrollment, `off` when no trigger is installed (`:3021`), else `self` |
| Stamped at **every** announcement | `stub_caps` (`signaling.rs:7208`, `:7221`) |
| Refused up front | `device_refusal` (`crates/modules/fleet/src/agent.rs:1427`): a 409 on one device (`:1474`), a `refused` row with its `refused_code` in a bulk (`:1606`) |
| Shown before the click | `ui/src/utils/updateRefusal.ts`; `AgentsSection.vue` disables "Update now" with the reason and lists those devices in the Update-all dialog |

> [!WARNING]
> **Stamped in `stub_caps`, not in the hello alone.** A heartbeat that
> re-announces changed caps (a login, a policy flip) *replaces* the stored blob
> (`AgentDao::update_capabilities`). A hello-only word would be erased at the
> device's first login, and the dashboard would count refused pushes as delivered
> again. The first draft had exactly that hole.

- ⚠️ **Unset means unknown, never a refusal.** No agent before P7 sends the word.
  An unrecognised word from a newer agent is ignored the same way. In both cases
  the push goes out as before.
- ⚠️ **`force` does not override it.** `force` overrides the server's own FR-2
  downgrade guard. The device ignores or drops the frame whatever the server sends.
- ⚠️ The codes are a compatibility surface with every dashboard: `primary_org`,
  `auto_update_off`, and FR-2's `pin_downgrade`.

## Phases

| # | Phase | Kill switch | Status |
|---|-------|-------------|--------|
| P1 | Stop waiting on a synchronously-completed install (Linux) | revert = today's unconditional wait | **shipped (#1400)** — field verification pending a release |
| P2 | Recover a `(deleted)` exe path + refuse a non-existent watcher binary + log the anyhow chain | revert = today's ENOENT | **shipped (#1427)** — ⚠️ NOT the guard it was scoped as: this is the whole reason the `.deb` path had no watcher |
| P3 | Read the verdict: startup log + `NodeStatus` + `roomler status` | pure addition; revert = today's silence | **built** (#2924, 2026-10-26) — field read owed on the release that carries it |
| P4 | macOS `AbandonProcessGroup` — **`com.roomler.update.plist`** is the job that matters (`update-helper` is that LaunchDaemon's body and is where `installer -pkg` is spawned) | packaging-only revert | proposed — see the note below |
| P5 | Fleet answer: report the outcome to the server and show it | render-only; absent field ⇒ today's blank | proposed |
| P6 | Transient systemd unit — **only if P1 leaves a measurable race** | `update_watcher_escape_cgroup`, default OFF | deferred, evidence-gated |
| P7 | The dashboard is told which devices will not act on an update ordered from it, and why: `update_mode` in the caps, refused up front with a `refused_code`, marked before the click | revert = today's blind push; an agent without the word is pushed as before | **shipped (#2545)** · field-verified on 0.4.146 + `hosted-20261019-e46da07` |

## Acceptance criteria

- [x] On a systemd host, a **real auto-update** leaves `last-install.json` in a
      terminal status. ⚠️ Show it is `InProgress` on the current release first — a
      pass proves nothing without the failing "before". **Before**, 2026-09-05:
      three cluster nodes at 0.4.70, all `InProgress`, ~54 releases stale (the
      P1 log). **After**, 2026-10-26: mars, jupiter and zeus — systemd,
      `KillMode=control-group`, the `.deb` path, 0.4.172 — all
      `SucceededVerified` for `agent-v0.4.172`, written at the moment of the
      2026-10-25 22:39Z update; on mars 13 of 13 organic updates since 0.4.155
      wrote a verdict (field log, 2026-10-26). Re-read the same day on
      **0.4.174**: all three again `SucceededVerified`, now 15 of 15 on mars
- [x] ⚠️ Verified on the **auto-update** path, not a manual foreground
      `roomlerd self-update` — that reaches a verdict today and hides the bug.
      mars's 0.4.172 install began `rc:agent.update — operator-triggered
      self-update` → `new release available — spawning installer and exiting`
      → `update install: package installed bin="apt-get"` → the watcher's
      verdict: the daemon's own updater, in the systemd unit, not a foreground
      command (field log, 2026-10-26). The 0.4.173 update that followed had
      **no push at all** — the daemon's own poll — and reached the same
      verdict on all three hosts
- [ ] A non-`SucceededVerified` outcome is visible without SSH: in the daemon's
      startup log and in `roomler status` — **built** (P3): the startup
      `last install:` line is WARN for every such outcome and `roomler status`
      leads with the verdict in capitals; the field read is owed on the release
      that carries it (a synthetic stale `InProgress` on one host, then
      `roomler exec <host> -- roomler status` and `journalctl -g "last install"`)
- [ ] **absent** (no install yet) and **`InProgress`** (an install that never
      reported) stay distinguishable — collapsing them recreates this bug one
      layer up — **built** (P3): two wire words (`Absent`, `InProgress`) and two
      sentences (`none recorded`, `NEVER REPORTED`), pinned by
      `summarize_keeps_absent_and_in_progress_apart` and
      `absent_never_reported_and_in_progress_are_three_sentences`; the field
      read (a deleted record beside a stale one, on one host) is owed with the
      criterion above
- [x] The `.deb` path is confirmed to spawn a watcher at all — **it did not,
      and the hypothesis was right.** 21 `post-install watcher spawn failed` in
      seven days against **zero** `post-install watcher started` (P2, #1427)
- [ ] A fleet query answers "did this update succeed on each host" without SSH
      (P5, open — the shape is in `docs/update-outcome.md` §8)
- [x] Windows behaviour is byte-for-byte unchanged — the install path, which
      is what this criterion guards: P1 kept the wait (`should_wait_for_installer`
      stays true for a live pid, and Windows passes `already_exited: false`), P3
      adds readers and touches nothing inside `watch()`. Field, 2026-10-26, the
      dev box (perMachine, attended, 0.4.172): `SucceededVerified`,
      `started_unix` 1792967937 → `finished_unix` 1792967960 — the watcher
      waited its 23 s on msiexec, then settled the service (`note: … service
      Roomler: running (0 starts; seen Running)`); again on 0.4.174 the same
      day, 17 s on msiexec and the same settle. ⚠️ What *did* change on
      Windows is not this FR's: the watcher's verdict print panicked on a
      closed exec pipe after each of those records was written (#2934), fixed
      by #2935 — the record, which is all P3 reads, was complete every time
- [x] Docs: [`docs/update-outcome.md`](../update-outcome.md) — the record and
      its fields, where `<log_dir>` lands per platform and role, the two Linux
      defects, the life of a verdict, the seven words and the three that must
      never collapse, who reads it from which process, the three surfaces, the
      field table — with diagrams, linked from `docs/README.md`;
      `docs/remote-control.md` §19.2's "future agent startups read the file" is
      true now and points there (#2924)
- [x] **P7**: an update ordered from a device's **secondary** org is refused with
      `primary_org` and the way out, and never counted delivered. ⚠️ The "before"
      is recorded: on 2026-10-19 the bulk route counted it delivered (field log)
- [x] **P7**: a device with auto-update off is refused with `auto_update_off`
- [x] **P7**: an agent that predates P7 is pushed exactly as before
- [x] **P7**: the word survives a heartbeat re-announce: after a login on the
      device, its row still carries `update_mode` (field-verified with a live
      config flip as the re-announce, not a login; see the field log)

## Out of scope

- A Unix `ensure_service_running` equivalent (`post_install.rs:550`). Windows needs
  it because the SCM does not auto-restart; Linux has `Restart=always` +
  `RestartPreventExitStatus=7 8`, and a watcher running `systemctl start` would
  fight the supervisor.
- Making `write_outcome` atomic — real, but a different failure mode from "never
  written".
- The FR-56 residuals; they stay as acceptance criteria on #1157.

## Open decisions

- **P5 shape**: an additive `#[serde(default)]` field on `ClientMsg::AgentHello`
  (matching `advertised_routes`) versus a dedicated report message. Hello is
  simpler; a message reports without waiting for a reconnect.
- **P6 at all?** Decide from P1's field data, not in the abstract.

## Field-verification log

| Date | What | Result |
|---|---|---|
| 2026-09-02 | Found while field-testing `agent-v0.4.48` | `last-install.json` stuck at `InProgress` on two hosts, one since 2026-08-29 for `agent-v0.4.16` while running 0.4.50 |
| 2026-09-02 | Preconditions measured on two distros | `systemd-run` present and `INVOCATION_ID` set in the unit process on both (Fedora/systemd 257, Ubuntu/systemd 255); `KillMode=control-group` on both |
| 2026-09-02 | ⚠️ Measurement hazard | `pgrep -x roomlerd` matched a **container's** process on a host running containerised test nodes (cgroup `/system.slice/docker-….scope`). A first reading wrongly showed `INVOCATION_ID` absent on Ubuntu. Probe `systemctl show roomlerd -p MainPID` instead |
| 2026-09-02 | ⛔ First design refuted before implementation | A systemd transient unit was the initial P1. Adversarial review found its safety premise false without `--service-type=exec`, its kill switch inert against the mode it guards, and that it would *create* a PID-reuse false-`Timeout` mode. The real defect is the self-pid wait (`updater.rs:1577` + `post_install.rs:221`) — removing it needs no systemd at all |
| 2026-10-19 | ❌ P7's "before", on the fleet | The operator's **Update all** counted a two-org aarch64 device (Fedora/Asahi) as pushed three times. Its journal logged `rc:agent.update ignored — only the primary enrollment drives the machine-wide self-updater` three times: the dashboard's org was its secondary enrollment. That day's remedy was `roomlerd org set-primary` on the device, which exposed #2540 (both meshes off; fixed in #2541) |
| 2026-10-19 | ❌ The "before" on a cell | A kept vmtest Ubuntu cell in two orgs, on 0.4.145 and the then-prod server `hosted-20261019-dd59ede`. A push to its secondary org's row: `HTTP 200 {"delivered":true}`, and in the same second the device logged `rc:agent.update ignored … ignored_total=1` |
| 2026-10-19 | ✅ **P7 field-verified on 0.4.146** | Server `hosted-20261019-e46da07`, candidate run 39331259421 (its `.deb` byte-identical to the published one). AC1: a push from the secondary org → `HTTP 409` naming `roomlerd org set-primary`, and 0 `rc:agent.update` lines on the device. AC2: `auto_update false` → row `update_mode=off` → `HTTP 409` (`auto_update_off`). AC3: a 0.4.145 agent (no word) → `HTTP 200 delivered`, as before. AC4: a live `keep_busy_enabled` flip made both connections re-announce their caps (`announcing changed capabilities on the heartbeat`), and both rows still carried the word. A lock/unlock first changed no caps, so it re-announced nothing and is not counted. A Win11 SYSTEM install laid over 0.4.145 reads `update_mode=self` |
| 2026-10-19 | ✅ **The field case itself, on the release** | The two-org aarch64 device from the top of this log, updated to 0.4.146 by its own updater (`.asc` verified against the pinned key; `post-install verdict: SucceededVerified`). Its secondary org's row now reads `update_mode=primary_org`, and a push from that org gets `HTTP 409` with the way out. Earlier the same day the same click was counted delivered three times while the device ignored it |
| 2026-10-26 | ✅ **P1 + P2 field-verified on the `.deb` path, the auto-update path, three hosts** | mars, jupiter, zeus: systemd units, `MainPID` live, `KillMode=control-group`, on 0.4.172. `/root/.local/share/roomler/logs/last-install.json` on each: `SucceededVerified`, `expected_version agent-v0.4.172`, `installer_exit_code 0`, `started_unix == finished_unix` (1792967957 / 1792967966), file mtime = the unit journal's `post-install verdict: SucceededVerified (new binary at /usr/bin/roomlerd reports roomlerd 0.4.172)` line (22:39:17Z, :17Z, :26Z). mars's journal over 14 days: **13 consecutive organic updates**, 0.4.155 → 0.4.172, each `post-install watcher spawned watcher_exe=/usr/bin/roomlerd staged=false` → `post-install watcher started … already_exited=true` → the verdict. Each began `rc:agent.update — operator-triggered self-update pin=None` → `new release available — spawning installer and exiting` → `update install: package installed bin="apt-get" euid=0`: the daemon's own updater, never a foreground `self-update`. Against the 09-05 "before" (three hosts `InProgress`, ~54 releases stale) this closes AC1 and AC2. P6 (the transient unit) stays unneeded: the race P1 left is not being lost here, 13 of 13 |
| 2026-10-26 | ✅ **Windows unchanged (AC7), and where the record lives there** | The dev box, perMachine, attended — first written here as "SystemContext"; corrected two rows below — 0.4.172. The record is the console-session **worker's** (`roomlerd run --supervisor scm`, session 1, the console user's token — it runs the updater, so its watcher wrote under that user's `%LOCALAPPDATA%\roomler\roomler\data\logs`): `SucceededVerified`, `started_unix 1792967937` → `finished_unix 1792967960`, `note … service Roomler: running (0 starts; seen Running)` — the 23 s wait on msiexec and the service settle, as before P1. The SYSTEM profile's copy of the file dates from July; the SCM host's `service-logs` dir has none. ⚠️ The LocalAPI `roomler status` reaches is the worker's (its `pid` 292 answered, `privileged:false`), so P3's reader in `DaemonState::status` resolves the dir the record is in. A reader in the SCM host would say `Absent` — honestly, naming `service-logs` |
| 2026-10-26 | ⚠️ One host off the sample | asahi-m2pro (Fedora, the tarball path) has `exec_enabled` off, so it could not be read this way; the 10-19 row above already saw its verdict line. An exec sweep is a biased sample — these three hosts prove the path, they do not count the fleet |
| 2026-10-26 | 🔨 **P3 built**, its field read owed | The read half: `NodeStatus.last_install`, the `last install:` startup line, the `last update` line of `roomler status`. Tests on both ends pin `Absent` apart from `InProgress`. ⏳ On the release that carries it: `roomler exec mars -- roomler status` → `last update ok — agent-v0.4.1xx installed … verified`, `journalctl -u roomlerd -g "last install"` → the INFO line; then on one host a stale `InProgress` written by hand and a restart → `NEVER REPORTED` + the WARN line (AC3), and the record deleted and a restart → `none recorded` (AC4) |
| 2026-10-26, 09:40Z | ✅ **Re-read on 0.4.174, three hosts** | mars, jupiter and zeus had each taken two more organic updates since the row above — 0.4.173 at 02:39Z, 0.4.174 at 09:03Z — and every record reads `SucceededVerified` for `agent-v0.4.174`, `installer_exit_code 0`, `started_unix == finished_unix` (1793005414 · 1793005401 · 1793005402), mtime = the journal's `post-install verdict:` line (09:03:34Z · 09:03:21Z · 09:03:22Z). Each: `new release available — spawning installer and exiting` → `update install: package installed bin="apt-get" euid=0` → `post-install watcher spawned watcher_exe=/usr/bin/roomlerd staged=false` → `watcher started … already_exited=true` → the verdict. ⚠️ The 0.4.173 update ran with **no `rc:agent.update` in the journal** (mars's pushes: 10-25 22:39Z, 10-26 07:35Z, 09:03Z): the daemon's own poll — AC2 in its purest form. mars: 15 of 15 since 0.4.155. Read over `roomler exec` with `MSYS_NO_PATHCONV=1` (Git-Bash otherwise rewrites `/root/…` into a Windows path before `roomler.exe` sees it) and with no `\|` in a `-g` pattern (the exec re-splits it into a shell pipe on the target) |
| 2026-10-26, 08:01Z | ✅ **Windows again, on 0.4.174 — and the panic beside every Windows record** | The dev box's worker record: `SucceededVerified` for `agent-v0.4.174`, `started_unix 1793001660 → finished_unix 1793001677` (17 s on msiexec), `note … service Roomler: running (0 starts; seen Running)`, written at 1793001692 once the service had settled. In the same second, `panic-58744-1793001692.log`: `failed printing to stdout: The pipe is being closed. (os error 232)` — and the 10-25 22:39Z record has its twin, `panic-30080-1792967976.log`, 16 s after its verdict. That is #2934: under an exec-driven `self-update` the watcher inherits the exec's stdout pipe, the install closes it, and the verdict `println!` — which runs **after** `write_outcome` — panicked. Pre-existing, not this FR's, fixed on master by #2935 (line-neutral in `main.rs`, so no anchor here moved); neither 0.4.173 nor 0.4.174 carries it. AC7 stands: the record, which is all P3 reads, was complete every time |
| 2026-10-26, 09:55Z | ✅ **The no-key state, on a live daemon** | The branch's `roomler` (built from 98dfc4cac) against the dev box's released 0.4.174 daemon, which predates P3: `roomler status` prints no `last update` line and `--json` carries no `last_install` key — silence, never `none recorded`. The third of the three states that must stay apart, read against a real daemon rather than the CLI's own test |
| 2026-10-26 | ❌→✅ **A label corrected: the dev box is attended, not SystemContext** | The 10-26 AC7 row first called the dev box "perMachine SystemContext". `roomler status` answers `mode service (per-user)` from `privileged: false` (`process_is_privileged` is `process_is_local_system()` on Windows, `agents/roomlerd/src/localapi_state.rs:401`), and `WorkerRole` names a worker `SystemContext` only when its own token is the LocalSystem SID (`agents/roomlerd/src/system_context/worker_role.rs:208`) — the record under the signed-in user's `%LOCALAPPDATA%` says the same. That is `docs/installation.md`'s "machine (attended)" row: perMachine MSI, SCM service, no SystemContext. The topology claim holds (the worker runs the updater, and the reader runs in the worker); the SystemContext mode — a LocalSystem worker whose `<log_dir>` is the SYSTEM profile's, where this box's July `InProgress` for `agent-v0.3.0-rc.183` still sits — is **unread**. Corrected in the spec, `docs/update-outcome.md` §5/§7 and the PR |

## P4 — scoped, and one question left before it is safe to write

Verified: **no** plist in `agents/roomlerd/packaging/macos/` sets
`AbandonProcessGroup`, so it defaults to false and launchd reaps the job's
remaining process group when the job exits — the same bug class as the systemd
cgroup teardown.

macOS is also the platform where it matters most, because it is the one where the
installer genuinely IS asynchronous: `installer -pkg` is `.spawn()`ed and returns
a live pid, so **P1 does not help there** — the watcher must actually wait, and
therefore must actually survive.

The job to fix is **`com.roomler.update`**, whose body is `roomlerd update-helper`
(`main.rs`, `Command::UpdateHelper`) — that is where the install runs.
`com.roomler.daemon` may need it too if the daemon ever spawns an installer
directly rather than delegating to the helper.

⚠️ Deliberately not written yet: `AbandonProcessGroup=true` stops launchd reaping
**every** remaining child, not just the watcher, and FR-41 already records a
grandchild holding a unit open. The open question is whether `update-helper`
exits immediately after spawning `installer(8)` — if so the key is exactly right;
if it waits, the reap window may not exist and the key would be loosening
shutdown for nothing. That wants checking on a real Mac (there is one on the
mesh) before a packaging change lands.

## Field-verification log — P1

| Date | What | Result |
|---|---|---|
| 2026-09-05 | The failing **"before"**, three hosts, prior to any fix | all `InProgress`: two stuck on `agent-v0.4.16` (written 2026-08-29) and one on `agent-v0.4.13` (2026-08-28), while all three were running **0.4.70**. ~54 releases, not one recorded outcome |
| 2026-09-05 | P2's `(deleted)` check | **INCONCLUSIVE, not negative** — `readlink /proc/<MainPID>/exe` is clean on all three, but the suffix can only exist between a `.deb` install and the next restart, and all three had restarted since. Needs running inside that window |
| 2026-09-05 | ⚠️ Probe hygiene | measured via `systemctl show roomlerd -p MainPID`, never `pgrep -x roomlerd` — the latter matches containerised test nodes and already produced one wrong reading in this FR |
| 2026-09-06 | ✅ **P2's question answered — and it inverts P1's evidence** | The `.deb` path has **never** had a watcher. On a cluster node: **21** `post-install watcher spawn failed` in 7 days, **0** `post-install watcher started`, `last-install.json` untouched since 2026-08-29 while the host moved 0.4.16 → 0.4.73. `apt` replaces `/usr/bin/roomlerd`, unlinking the running image, so `current_exe()` reads `…(deleted)` and `Command::spawn` fails ENOENT. 🔑 The code asserted the opposite — *"Unix package managers replace files without stopping readers, so the in-place spawn stays correct there"* — and replacing the file is exactly what breaks it. ⚠️ **This corrects what P1's "before" evidence meant**: the frozen `InProgress` records were read as the cgroup killing the watcher; on this path there was no watcher to kill. P1 still fixes the *tarball* path, where the watcher does start. |
| 2026-09-06 | ⚠️ Why it hid for months | the failure surfaced as a single context-less `WARN` — `%e` on an `anyhow::Error` prints only the outermost context (*"spawning post-install-watch subprocess"*) and drops the ENOENT that explains it. P2 logs the chain (`{:#}`). A diagnostic that cannot say *why* is how 21 consecutive failures read as noise. |
| 2026-09-06 | ⏳ P1 field verification still open | P1 is in released 0.4.72/73/74 and the fleet is on 0.4.73, but every host checked takes the `.deb` path — where P2 had to land first for a watcher to exist at all. Re-verify on a **tarball** host, or on any host once P2 ships. |
| 2026-10-26 | ✅ **Closed, on the `.deb` path** | With P2 in the fleet since 0.4.75, the same three cluster nodes at 0.4.172 each hold `SucceededVerified` for `agent-v0.4.172`, and one host's journal shows 13 of 13 organic updates reaching a verdict — 15 of 15 by the same day's 0.4.174 re-read (the main log above). The tarball path is still unread this way: the one tarball host has exec off; its 10-19 verdict line is the evidence there |
