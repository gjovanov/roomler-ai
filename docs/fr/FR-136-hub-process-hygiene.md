# FR-136: Hub hygiene — a stopped task leaves nothing running, the host sets the pace, and workers get mail

**Issue:** [#3457](https://github.com/gjovanov/roomler-ai/issues/3457) · **Status:** claimed 2026-11-04.
P0–P8 shipped the same day. P9–P11 came out of the field runs: the navigator, the pane that shrank
around it, and hub-guard 0.4.0–0.4.3. AC1–AC8 and AC10 are field-verified; AC9 waits on its
day-later read, now that all three hubs run guarded. Builds on
[FR-131](FR-131-hub-coordinator.md), the hub. The engineering reference is
[`agent-hub.md` §12](../agent-hub.md#12-process-hygiene-the-host-budget-the-mailbox-and-the-idle-handoff),
and §8 for the view.

## Problem / Goal

On 2026-11-03 a hub worker passed a docs-anchor helper its file list inline, where the helper expects a
file that lists the paths. The helper read a Rust source file as the list. It ran
`git diff … -- "<line>"` through `sh` once per line. A doc comment's `` `yes` `` code span ran `yes`
inside a command substitution, which never returns.

- The worker stopped the task with TaskStop, which answered "Successfully stopped".
- An hour later the hub rotated the worker, and its session ended.
- The shell kept a core busy and grew to 2.35 GB private memory for 1 h 45 m, with Memory Compression
  and System busy behind it, until it was found by hand.
- Nothing in the hub could see it.

The same week showed the rest of the shape:

- **TaskStop leaving what a task started** was already on record three times: a build wrapper that
  later won a slot and compiled, a `gh` started by a stopped gate script, and a narration loop.
  Each was filed under its own topic, so no worker had one rule.
- **The three hubs' caps allow 23 sessions together**, while the host carries about 15. Each hub keeps
  its own build-slot pool of 2, and a worker built outside its pool.
- **After a host reboot, 13 workers were marked running**, and the hubs would restart them all at once.
- **Hub messages reach workers as cross-session sends.** A send expires unread when the two sessions'
  permission modes differ, and a send to a stopped worker restarts it.

**Goal.** A task, a call or a worker that stops leaves nothing running. The hubs together never ask the
host for more than it has. The hub view shows each worker's processes and the host's load, opens
workers and their PRs directly, and delivers mail that survives a mode mismatch and never wakes a
stopped worker. A session that sits idle hands off before its prompt cache goes cold.

## Evidence — the mechanics the design rests on

| Fact | Measured how |
|---|---|
| TaskStop ends only the tool's own shell: a perl → sh → sleep chain survived, from an elevated session and from one that was not | a bounded repro (`sleep 271` in place of `yes`), twice |
| A Bash call that hit its timeout went to the background, and after TaskStop its `du` and `tail` kept running | an ordinary command, by accident |
| Git Bash (MSYS2) makes a new Windows process at every exec, so each survivor's Windows parent is an exited stub, and a Windows tree kill cannot reach it | the parents' PIDs did not exist while the chain ran |
| MSYS2's own table keeps the chain and its process group; one `kill -TERM -- -<PGID>` ended perl, sh and sleep together | `ps -l`, before and after |
| Each Bash call runs as claude → launcher `bash.exe` (native) → `usr/bin/bash` (MSYS2 PPID 1) → the command, in a process group of its own (the shell snapshot sets `set -o monitor`) | process listings of live calls |
| Every child of a session's Bash inherits `CLAUDE_PID` and `CLAUDE_CODE_SESSION_ID`, readable from `/proc/<pid>/environ` without elevation, a native child included | read on the repro chain |
| A bare `bash` on the Windows `PATH` is WSL's | `PATH` order |
| The function-hooks API (2.1.314): `tool.call` wraps every call and can deny it, rewrite it or attach context; `$.env.set` reaches later Bash children; `command.run` sees a plugin's `$.command.run` and can answer in its place; `session.end` shares one 1.5 s bound across every hook; `$.fs` cannot rename | the API's typings |
| The third-party auto-handoff plugin (0.1.0) fires once per idle period, re-armed only by a person, and dispatches through `$.command.run`; by default it writes `docs/handoffs/` inside the project | its source |

## P1 results — the platform, 2026-11-04

Each check ran in a throwaway session before anything relied on it.

| Check | Result | What the design uses |
|---|---|---|
| A plugin in the skills folder loads in a `claude --bg` session started the way `spawn` starts one | pass: a scratch plugin's `session.start` wrote its marker | no `--plugin-dir` needed |
| `CLAUDE_CODE_ENABLE_FUNCTION_HOOKS` holds under `--settings '{"env":…}'` | pass: the `--settings` env merges with the user's | `spawn` passes the flag anyway |
| `$.env.set("HUB_CALL", …)` before `next(e)` reaches the call's children, and a `$.env.set` `WSLENV` carries it into WSL | pass | the call tags |
| `$.env.get("CLAUDE_PID")` names the session's own claude process | **fail**: it returned an inherited value, another process's pid | the fallback: the pid from `claude agents --json` for `$.session.id()` |
| `$.prompt.submit` from a timer works in a `--bg` session with no client attached | pass: the turn ran, and its transcript row reads `origin.kind: plugin` and `promptSource: system` | idle mail delivery; `hub.mjs`'s scan skips such rows |

## Key design

| Piece | Where | What it does |
|---|---|---|
| **hub-guard** (new, a function-hooks plugin loaded in every session) | `~/.claude/skills/hub-guard/` | Tags each Bash call through `$.env.set`, so no command is ever rewritten and no permission rule or plan-mode check changes. Learns each background task from `backgroundTaskId`. On TaskStop it ends exactly that task's tree: MSYS2 process groups first, then native subtrees and WSL, each process re-read just before it is killed. It does the same for an interrupted call, a Monitor deadline and a subagent's end, and puts one line in the transcript. In workers it refuses a bare cargo build, delivers mail, and gates the idle handoff. |
| **The hub helper** | `~/.claude/skills/hub/hub.mjs` | Spawns workers with markers (`HUB_WORKER`, `WSLENV`) and runs `procs` and `reap`. Cleans a stopped or rotated worker from outside, on Windows and in WSL. Adds LEFTOVER, RUNAWAY, HOT and MAIL_WAITING, the host budget (`~/.claude/hub/host.json`), staggered respawn, `post`, and `role`. |
| **Build slots** | `~/.claude/skills/hub/cargo-slot.sh` | One host-wide pool with FIFO tickets. The holder sends a heartbeat, and a slot is reclaimed only when its owner is dead. |
| **The navigator** `hubs` (P9) | `~/.claude/skills/hub/hubs.mjs`, run as `hubs` in a terminal | Every hub on two lines; Enter shows one hub's workers grouped by what they need, with per-worker CPU, memory and leftovers; `/` filters every hub and worker; a worker's details; answer, clean up, open the worktree, the PR or the issue; Enter attaches in the same window, and Ctrl+Z comes back. |
| **The hub view** | `~/.claude/skills/hub-view/` | The pane in the chat: since P10, the host strip and one question list across hubs, and nothing per worker. A question's Answer opens its worker view: the worker's processes with Clean up, its background tasks, Attach in a new terminal tab, Open for the worktree, the PR or the issue, and the mail states. |
| **The idle handoff** | `~/.claude/skills/hub-handoff/`, plus the auto-handoff plugin | A worker writes `handoff-g<N>.md` and reports HANDOFF READY. A hub writes `hub-handoff.md`. Any other session writes outside every repository. A gate skips it, with no model turn, on a cold cache, for a parked or done worker, or while the hub's tick is fresh. |

**Never force-killed:** a test daemon (force-killing one can install the live release), anything marked
`HUB_KEEP=1`, browsers, editors and terminals.

The operator's own sessions get the per-task cleanup only. A session-end sweep keyed on `CLAUDE_PID`
would also end browsers, tunnels and servers first started from them, and the session-end bound cannot
hold a reaper.

## Phases

| Phase | Scope | Status | Kill switch |
|---|---|---|---|
| P0 | Contain the auto-handoff plugin's defaults: a global git exclude for `docs/handoffs/`, the `hub-handoff` skill, `hub.mjs role`, and the plugin pointed at the skill | shipped 2026-11-04 | the plugin's `command` back to its own, or the plugin disabled |
| P1 | Prove the platform in throwaway sessions: plugins in a background session, the hooks flag under `--settings`, `$.env.set`, `$.prompt.submit` with no client attached | passed 2026-11-04, one fallback ([P1 results](#p1-results--the-platform-2026-11-04)) | — |
| P2 | hub-guard cleanup (TaskStop, interrupt, Monitor, subagent end) | shipped 2026-11-04, hub-guard 0.1.2 | `~/.claude/hub-guard/off`, or its `cleanup` part alone |
| P3 | The hub helper: markers, `procs`, `reap` on stop, rotation and tick, the new signals, the host budget with staggered respawn, one FIFO slot pool, the cargo guard | shipped 2026-11-04 | `host.json` limits; the guard's `cargo` part |
| P4 | The rules in the worker protocol and the hub skill | shipped 2026-11-04 | — |
| P5 | The hub view | shipped 2026-11-04, hub-view 0.3.0 (52 engine-kit tests) | the previous `hub-view` version |
| P6 | The mailbox | shipped 2026-11-04, hub-guard 0.3.0 | the guard's `mail` part; cross-session sends stay the fallback |
| P7 | The idle-handoff gate, then 45 minutes | shipped 2026-11-04, hub-guard 0.2.0; `minutes` is 45 | the guard's `gate` part; `minutes` back to 50 |
| P8 | Docs: `docs/agent-hub.md` and its `docs/README.md` row | the docs PR, 2026-11-04 | — |
| P9 | The navigator `hubs`: every hub, then a hub's workers, then one worker, in one terminal window. The operator's look at the pane found the full board too much at once, with no search, and Attach opening another window | shipped 2026-11-04, hub skill `2823da6`–`1a1eb2d`, 11 `node --test` cases | the pane's lists come back with `hub-view` 0.3.0 |
| P10 | The pane shrinks to the host strip and the open questions. `/hub` answers the same as text, and `/hub-view status` keeps the whole board | shipped 2026-11-05, hub-view 0.4.0, 53 engine-kit tests | `hub-view` 0.3.0 |
| P11 | hub-guard from the field. 0.4.0: the cleanup line for a Monitor deadline rides on the next tool result, and the task registry survives a reload. 0.4.1: one native process's environment can no longer crash the `/proc` scan. 0.4.2: an interrupt that Claude Code reports as a refused call starts the sweep. 0.4.3: the sweep runs from a timer, outside the call | shipped 2026-11-04 to 2026-11-05, hub-guard 0.4.0–0.4.3 | `~/.claude/hub-guard/off`, or the previous version |

## Acceptance criteria

- [x] **AC1 — a stopped task leaves nothing.**
  - TaskStop on the 2026-11-03 shape (perl → sh → a child that never ends) ends every process the task
    started, and the transcript says so in one line.
  - The same holds for a call that timed out into the background, an interrupted call, and a Monitor
    task at its deadline.
  - A subagent's background command outlives the subagent's answer, since Claude Code waits on it, and
    stopping it from the main session ends what it started.
  - *2026-11-04 and 2026-11-05:*
    - *The 2026-11-03 shape and a call that timed out into the background, each ended whole (P2). A
      timed-out call's stop ended its WSL child too: `ended 2 processes … (bash, python3.12 (WSL))`
      (13:56Z).*
    - *A Monitor deadline, swept 15 s after it. With hub-guard 0.4.0 its line rode on the next tool
      result: `hub-guard cleanup, after Monitor …'s deadline: ended 3 processes … (sleep×2, perl).`
      (14:25:39Z → 14:26:07Z).*
    - *A subagent's background command, stopped from the main session: `ended 3 … (sleep, bash,
      perl)` (14:03Z).*
    - *An interrupted call, with the Esc injected into a throwaway session's console, on hub-guard
      0.4.3: nothing was left with its tag, and the next result read `hub-guard cleanup, after the
      interrupt: ended 5 processes … (sh×2, find×2, perl).` (2026-11-05 00:04Z).*
- [x] **AC2 — only that task.**
  - Of two parallel background calls, stopping one leaves the other running.
  - A server started with `&` in an earlier call survives.
  - Nothing on the never-kill list, and nothing marked `HUB_KEEP=1`, is force-killed.
  - No command is rewritten: an allow rule still matches in a default-mode session.
  - *2026-11-04 13:45–13:53Z:*
    - *Of two parallel calls, stopping A `ended 3 … (perl, bash, sleep)`, and B kept running.*
    - *A node server started with `&` in an earlier call still answered afterwards.*
    - *Stopping C kept a test `roomlerd` (`never killed`) and a sleep started with `HUB_KEEP=1`, and
      its line named both.*
    - *A default-mode worker ran `printenv HUB_CALL` through an allow rule, with no prompt.*
- [x] **AC3 — a stopped or rotated worker leaves nothing**, on Windows and in WSL, forked databases
  included unless kept. The tick finds and reports what a hard kill or a reboot left.
  - *2026-11-04:*
    - *`hub.mjs stop` ended 5 processes, one of them in WSL (P3).*
    - *A throwaway worker's stop ended 12 (14:05:27Z), a forked WSL server among them: a key-value
      store that forks the way `mongod --fork` does. Its `HUB_KEEP=1` twin was kept, and still
      answered.*
    - *A real rotation and a real stop reaped `ended 0` on both sides (08:02Z, 11:26Z), and another
      rotation `ended 3 (bash, sleep, bash)` (16:50:47Z).*
    - *A planted leftover of an ended generation was listed by `procs` and ended by Clean up
      (09:03Z), and a worker's LEFTOVER showed on its hub's board (11:58Z).*
- [x] **AC4 — the board names a runaway.** A live `yes`, an MSYS2 shell over 1 GB, or a worker over 4
  cores for 10 minutes shows as RUNAWAY or HOT with the worker it belongs to.
  - *2026-11-04:*
    - *RUNAWAY on a live `yes` (11:58Z).*
    - *RUNAWAY on a bash holding a 1.1 GB string: `▶ W-1 RUNAWAY: bash pid … at 1 GB` (13:59:51Z).*
    - *HOT: `· W-1 HOT: its processes average 5 cores over 10m (sha256sum×5, sleep, timeout×5)`
      (14:05:06Z).*
- [x] **AC5 — the host sets the pace.**
  - Dispatch is held, with the reason, when live sessions reach the host cap, when free memory or
    commit falls below the floor, or when CPU stays over the cap for a minute.
  - After a reboot, workers come back a few per tick.
  - Builds take one host-wide FIFO pool, and a bare build in a worker is refused with the command that
    would run it.
  - *2026-11-04:*
    - *The session cap held a live hub's dispatch (11:25Z).*
    - *Against a scratch `host.json`, so no live hub held: `HOLD: dispatch held: ⛔ host: 51 GB free
      (RAM or commit) < 60` (14:00:41Z), and `host: CPU 32% ≥ 20% over the last minute` (14:01:44Z).*
    - *After the reboot, 2 stopped workers were `act` and 5 were `info` (P3).*
    - *The one host-wide pool held 2 of 5 waiters (P3).*
    - *A worker's bare `cargo check` was refused with the slot command.*
- [x] **AC6 — the view.**
  - The navigator `hubs` lists every hub, then a hub's workers grouped by what they need, with
    per-worker CPU, memory and leftovers. It filters across every hub and shows a worker's details.
  - From it the operator answers, cleans up, opens the worktree, the PR and the issue, and attaches in
    the same window.
  - The pane in the chat keeps the host strip and one question list across hubs. A question opens its
    worker view: its processes with Clean up, its background tasks, attach in a new terminal tab, and
    open.
  - *2026-11-04:*
    - *The pane's Open ×3, Attach and Clean up, with the operator watching (11:25Z).*
    - *The operator's look at the full board failed it (13:42Z), and the operator chose a navigator in
      its place (14:12Z, decision D5).*
    - *The operator's live try (16:09:44Z): `hubs`, then a throwaway hub, then its worker, attached.
      "demo test" typed there was answered "GOT: demo test" 2 s later, and Ctrl+Z came back: "Worked
      end to end".*
  - *2026-11-05 00:29:55Z, hub-view 0.4.0: a fresh session's `/hub` answered, with no model turn, the
    host strip with its hold, four open questions and `5 hubs · 77 workers, 4 running · browse them
    with hubs in a terminal`.*
- [x] **AC7 — mail.**
  - It reaches a busy worker within one tool call and an idle one within 15 s, in any permission mode.
  - A stopped worker keeps its mail and is not restarted by it.
  - Nothing is delivered twice.
  - Mail never reads as the operator typing.
  - *2026-11-04:*
    - *Idle in 5 s; posted while stopped, it waited with no respawn, and a respawn delivered it (P6,
      07:54–07:59Z).*
    - *In a default-mode worker:*
      - *posted mid-`sleep 20`, it rode on that call's own result 1 s later (13:52:55Z);*
      - *posted while idle, it arrived as a turn 1 s later, `origin: plugin hub-guard` (13:54:27Z).*
    - *The mailbox read `0 waiting · 0 claimed · 2 delivered`, one context row each.*
- [x] **AC8 — the idle handoff.**
  - A worker that waits with a big context while its hub's tick is stale writes `handoff-g<N>.md` and
    reports HANDOFF READY.
  - It is skipped, with no model turn, for a parked or done worker, under a fresh hub tick, and on a
    cold cache.
  - A session that is neither hub nor worker writes its handoff outside every repository.
  - *2026-11-04:*
    - *A real worker, 383k context, waiting on CI, its hub's last tick an hour old:*
      - *the idle run at 16:47:39Z wrote `handoff-g4.md` (16:50:22Z) and sent HANDOFF READY
        (16:50:39Z);*
      - *its hub rotated it 8 s later.*
    - *Skipped with no model turn:*
      - *a parked worker (P7);*
      - *`its hub ticked 0 min ago and acts on the cache itself.` (14:04Z);*
      - *`the prompt cache is already cold (1h tier)` on a real worker (07:40:39Z, 13:15:00Z).*
    - *A session that was neither wrote its handoff under `~/.claude/handoffs/` (17:03:30Z).*
- [ ] **AC9 — field.** The hubs bb, bl1 and unhive resume on it. The first tick shows the host line and
  the badges, respawns are staggered, and a real TaskStop in a worker carries its cleanup line. A day
  later nothing is left behind.
  - *2026-11-04:*
    - *bb and bl1 resumed on it at 09:01Z, the host line on their first tick, and their respawns
      staggered.*
    - *A real worker stopped its `gh pr checks --watch` task, and its transcript read `hub-guard
      cleanup: ended 3 processes task … had started (bash, gh, timeout).` (12:12:45Z).*
  - *2026-11-05: unhive's hub, whose process predated the install, came onto the guard at a
    `/reload-plugins` (01:14:49Z): its next background calls were recorded with their tags. It then
    moved to a fresh process at 01:23Z (decision D7).*
  - *Owed: the day-later read.*
- [x] **AC10 — docs.** `docs/agent-hub.md` gains process hygiene, the host budget, the mailbox and the
  idle handoff, in house style (mermaid, tables), and its row in [`docs/README.md`](../README.md)
  says so. *2026-11-04: [§12](../agent-hub.md#12-process-hygiene-the-host-budget-the-mailbox-and-the-idle-handoff),
  with a flowchart and a sequence diagram, the new signals in §5, the view's new parts in §8, and the
  index row.*

## Open decisions

Decided by the operator on 2026-11-04:

| # | Question | Decision |
|---|---|---|
| D1 | Who builds it | One session directly, while the hubs are down after the 2026-11-03 reboot. |
| D2 | What a stopped worker's leftovers get | Killed, on Windows and in WSL, forked databases included, except what is kept or never-kill. The review refined this: the operator's own sessions get only the per-task cleanup. |
| D3 | Which hub-to-worker messages go through the mailbox | All of them, and the view's answers too. |
| D4 | Where the idle handoff runs | In every session. Hubs and workers use the hub's own format; other sessions write outside every repository. |
| D5 | What AC6's view becomes, after the full board failed the operator's look | A standalone navigator in the terminal, like `claude agents`: hubs at the top, Enter into one hub's workers, attach in the same window (P9). The pane shrinks to the host strip and the open questions (P10). |
| D6 | How AC1's interrupted call is tested, after the operator's Esc never reached the session in five tries | Unattended: a throwaway interactive session in a console of its own, with the Esc written into that console's input. |
| D7 | How unhive's hub gets the guard, since its process predates the install | A fresh process at its next context handoff: `/exit`, `claude`, `/hub resume unhive`. A `/reload-plugins` had loaded the guard into it first (field log, 2026-11-05 01:14). |

## Out of scope

- Mail from a worker to its hub, which stays a cross-session send.
- The other docs-anchor helpers kept outside the repository. The one that bit was fixed on 2026-11-04:
  no shell, and every entry must be a path.
- Splitting hive out of the daemon's crate, which is scoped separately.
- Changes to the third-party auto-handoff plugin.

## Field log

| When (UTC) | What |
|---|---|
| 2026-11-03 21:37 | The runaway starts. |
| 2026-11-03 22:08 | TaskStop answers "Successfully stopped"; the chain keeps running. |
| 2026-11-03 23:22 | Found and ended by hand with one process-group kill. |
| 2026-11-04 | Repro: TaskStop leaves a bounded perl → sh → sleep chain, elevated or not. A `du` that timed out leaves `du` and `tail`. |
| 2026-11-04 | P0: the idle handoff can no longer write inside a project. |
| 2026-11-04 | P1: four of the five platform checks pass; `CLAUDE_PID` from the hooks worker is inherited, so the session's pid comes from `claude agents`. |
| 2026-11-04 | P2, live: a TaskStop on the 2026-11-03 shape, and on a call that timed out into the background, each ended the whole chain, and the transcript read `hub-guard cleanup: ended 4 processes … (sh, perl, sleep, bash)`. The first run caught the reaper killing itself, since it inherited the latest call's tag; it now skips itself, its ancestors and every other run of itself. A native subtree, the tag race between parallel calls (the tree's own tag decides), the never-kill list and WSL `nohup` and `setsid` children were each shown. |
| 2026-11-04 | P3, live: `hub.mjs stop` on a worker ended 5 processes, one in WSL, in 3.2 s. A worker's bare `cargo check` was refused with the slot command. After the reboot, one hub showed 2 stopped workers as `act` and 5 as `info`, with the host line on its board; the shared slot pool held 2 of 5 waiters. |
| 2026-11-04 | P7, live: the gate skipped a parked worker's idle handoff with no assistant turn, and let a plain session's through. `minutes` set to 45. |
| 2026-11-04 07:54–07:59 | P6, live, on a throwaway worker: mail posted while it was idle was delivered in 5 s, as a turn of plugin origin. Posted while it was stopped, mail waited with `MAIL_WAITING` and no respawn; a respawn delivered the waiting message and its RESUME together, 17 s later. `status --json --peek` on the five live hubs differed from the old helper only by the new mail counts. |
| 2026-11-04 08:02 | Field, a working hub: the first rotation on the new helper, W-2 g1 → g2. `stop g1 … for rotation (ok)`, then `reap g1 (…/W-2/g1, pid …): ended 0`, on Windows and in WSL, and g2 started 1 s later with a mailbox, its hub-guard recording its background tasks. |
| 2026-11-04 | P5: hub-view 0.3.0, 52 engine-kit tests. A `--bg` session's `/hub-view status` drew the host strip and one questions line across the five live hubs. Attach, Open and Clean up are covered by the kit only, since running them live opens windows on the operator's desktop. |
| 2026-11-04 07:42–16:33 | Every session started after hub-guard's install (about 01:59) tags its calls, and **a `/clear` does not load the plugin into an older process**: three `/clear`s after the install (07:42, 08:38, 12:04) left their processes untagged. unhive's hub, started before the install, ran untagged too: its background call at 16:19:47 carried no `HUB_CALL`, and nor did any of the 7 processes of its own seen from 16:26 to 16:33. A `/login` loaded the plugin once, but it also switched that process's account, so it is no method. Decision D7. *Corrected 2026-11-05: this row first said a `/reload-plugins` at 16:10 had failed to load the plugin. That command never reached unhive's hub, whose transcript has no such row. See 2026-11-05 01:14.* |
| 2026-11-04 09:01–12:13 | AC9 starts. Hubs bb and bl1 resume on the new helper and on hub-guard at 09:01, the host line on their first tick. At 12:12:45 a real worker's TaskStop of its CI watch carries its line: `ended 3 processes … (bash, gh, timeout)`. |
| 2026-11-04 13:42–14:12 | AC6: the operator's look at the full board failed it. It showed too many hubs and workers at once, with no search, and Attach opened another window. The operator chose a standalone navigator (D5). |
| 2026-11-04 13:42–22:58 | The operator's Esc never reached a running call in a VS Code CLI session, in five tries. In Windows Terminal it landed on an approval prompt, which refuses the call before it runs. Decision D6. |
| 2026-11-04 13:45–14:05 | AC2–AC5 and AC7, on throwaway workers and a scratch `host.json`, so no live hub held or lost anything. Each criterion's note above has its lines. |
| 2026-11-04 14:03 | **A subagent's background command now outlives its final answer.** Claude Code waits on it (`endsWithFinal: false`), so there is no end for hub-guard to sweep. Stopped from the main session, it ended 3 (`sleep, bash, perl`). AC1's clause is reworded to match. |
| 2026-11-04 14:24 | P11, hub-guard 0.4.0. A Monitor deadline's sweep had left no line, since no tool result carried it. Its line now rides on the next result: `hub-guard cleanup, after Monitor …'s deadline: ended 3 processes … (sleep×2, perl).` (14:26:07). The task registry survives a reload, where a deploy used to wipe it. |
| 2026-11-04 14:36–16:09 | P9, the navigator `hubs`. The operator's first try (15:25) caught arrows that did not move, a swallowed attach and a restart nobody had asked for. The second try worked end to end (16:09:44): "demo test" came back as "GOT: demo test", and Ctrl+Z returned to the navigator. |
| 2026-11-04 15:47–22:10 | Hub bb's ticks skipped `hub.mjs status` from 15:47 to 22:06, so its board went stale and the new context cap could not reach its workers. Told at 22:10, it ran `status` again. A tick that skips `status` turns every signal off without a sign. |
| 2026-11-04 16:00 | The operator re-tiers the hubs by need. Sonnet runs at high and Opus at high or xhigh, beside the CLAUDE.md §3 floor of Opus at max, and 14 workers move. Each hub's context cap becomes 200k soft and 300k hard (20 % and 30 % of a 1M window), the helper's default since. A tier binds at spawn or rotation, never at a respawn: a bb worker came up on its new tier at its 16:50 rotation. |
| 2026-11-04 16:47–17:03 | AC8 in the field. A real worker's idle handoff ran at 16:47:39, its HANDOFF READY came at 16:50:39, and the rotation followed at 16:50:47. A session that is neither hub nor worker wrote its handoff under `~/.claude/handoffs/` at 17:03:30. |
| 2026-11-04 22:21 | P11, hub-guard 0.4.1, from hub bb's report. One native process's environment, seen on a PowerShell and on Blender, kills any MSYS2 reader of `/proc/<pid>/environ` with "cmalloc would have returned NULL". It took the whole scan down, so a reap could read "nothing left running" while processes ran. A scan with bash builtins alone crashed too. `hg.pl` now scans in children of 40 pids, reports such a pid as `unreadable`, and `procs` and `reap` say so. |
| 2026-11-04 23:43 – 2026-11-05 00:04 | P11, hub-guard 0.4.2 and 0.4.3. **Claude Code 2.1.314 answers Esc on a running call exactly as it answers a refused prompt.** The result is an error, `Exit code 137` with `[Request interrupted by user for tool use]`, and never `interrupted: true`, so the interrupt branch had never run live. 0.4.2 sweeps on that shape. Its sweep ran inside the interrupted call and died with it, leaving perl, sh×2 and find. 0.4.3 runs the sweep from a timer 1.5 s later. The unattended run (D6), at 00:04:23, ended `sh, find, sh, perl, find`, left nothing with the tag, and the next result carried the line. |
| 2026-11-05 00:28–00:30 | P10, hub-view 0.4.0, live. A fresh session's `/hub` answered the host strip, four questions and `5 hubs · 77 workers, 4 running · browse them with hubs in a terminal`, with no model turn (00:29:55). The first try went wrong. It passed `/hub` as a background session's prompt from Git Bash, which rewrote it into a path, `C:/Program Files/Git/hub`. That session started a model turn on the path and was stopped within 25 s, having only read files. `MSYS_NO_PATHCONV=1` keeps a slash command a slash command. |
| 2026-11-05 01:14–01:55 | **`/reload-plugins` does load a newly installed plugin into a running session**, and the 2026-11-04 07:42 row is corrected. unhive's hub process, started before the install, had left every background call unrecorded: 6 from 16:09 to 23:44, and 2 at 00:13 after a `/clear` at 23:57. After a `/reload-plugins` at 01:14:49, its next two background calls (01:19:07, 01:19:09) were recorded with their tags. Then the operator moved the hub onto a fresh process (D7). A new session resumed it at 01:23:18, and at 01:55:32 its call's `bash` and `gh` carried `HUB_CALL=4cf14264`. All three hubs now run guarded. |
