# FR-136: Hub hygiene — a stopped task leaves nothing running, the host sets the pace, and workers get mail

**Issue:** [#3457](https://github.com/gjovanov/roomler-ai/issues/3457) · **Status:** claimed 2026-11-04.
P0 shipped the same day. Builds on [FR-131](FR-131-hub-coordinator.md), the hub.

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

## Key design

| Piece | Where | What it does |
|---|---|---|
| **hub-guard** (new, a function-hooks plugin loaded in every session) | `~/.claude/skills/hub-guard/` | Tags each Bash call through `$.env.set`, so no command is ever rewritten and no permission rule or plan-mode check changes. Learns each background task from `backgroundTaskId`. On TaskStop it ends exactly that task's tree: MSYS2 process groups first, then native subtrees and WSL, each process re-read just before it is killed. It does the same for an interrupted call, a Monitor deadline and a subagent's end, and puts one line in the transcript. In workers it refuses a bare cargo build, delivers mail, and gates the idle handoff. |
| **The hub helper** | `~/.claude/skills/hub/hub.mjs` | Spawns workers with markers (`HUB_WORKER`, `WSLENV`) and runs `procs` and `reap`. Cleans a stopped or rotated worker from outside, on Windows and in WSL. Adds LEFTOVER, RUNAWAY, HOT and MAIL_WAITING, the host budget (`~/.claude/hub/host.json`), staggered respawn, `post`, and `role`. |
| **Build slots** | `~/.claude/skills/hub/cargo-slot.sh` | One host-wide pool with FIFO tickets. The holder sends a heartbeat, and a slot is reclaimed only when its owner is dead. |
| **The hub view** | `~/.claude/skills/hub-view/` | A host strip; per-worker CPU, memory and leftovers; one question list across hubs; a worker's processes with Clean up, and its tasks. Attach in a new terminal tab; open the worktree, the PR or the issue. Mail states. |
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
| P1 | Prove the platform in throwaway sessions: plugins in a background session, the hooks flag under `--settings`, `$.env.set`, `$.prompt.submit` with no client attached | — | — |
| P2 | hub-guard cleanup (TaskStop, interrupt, Monitor, subagent end) | — | `~/.claude/hub-guard/off`, or its `cleanup` part alone |
| P3 | The hub helper: markers, `procs`, `reap` on stop, rotation and tick, the new signals, the host budget with staggered respawn, one FIFO slot pool, the cargo guard | — | `host.json` limits; the guard's `cargo` part |
| P4 | The rules in the worker protocol and the hub skill | — | — |
| P5 | The hub view | — | the previous `hub-view` version |
| P6 | The mailbox | — | the guard's `mail` part; cross-session sends stay the fallback |
| P7 | The idle-handoff gate, then 45 minutes | — | the guard's `gate` part; `minutes` back to 50 |
| P8 | Docs: `docs/agent-hub.md` and its `docs/README.md` row | — | — |

## Acceptance criteria

- [ ] **AC1 — a stopped task leaves nothing.**
  - TaskStop on the 2026-11-03 shape (perl → sh → a child that never ends) ends every process the task
    started, and the transcript says so in one line.
  - The same holds for a call that timed out into the background, an interrupted call, a Monitor task
    at its deadline, and a subagent's background command at its end.
- [ ] **AC2 — only that task.**
  - Of two parallel background calls, stopping one leaves the other running.
  - A server started with `&` in an earlier call survives.
  - Nothing on the never-kill list, and nothing marked `HUB_KEEP=1`, is force-killed.
  - No command is rewritten: an allow rule still matches in a default-mode session.
- [ ] **AC3 — a stopped or rotated worker leaves nothing**, on Windows and in WSL, forked databases
  included unless kept. The tick finds and reports what a hard kill or a reboot left.
- [ ] **AC4 — the board names a runaway.** A live `yes`, an MSYS2 shell over 1 GB, or a worker over 4
  cores for 10 minutes shows as RUNAWAY or HOT with the worker it belongs to.
- [ ] **AC5 — the host sets the pace.**
  - Dispatch is held, with the reason, when live sessions reach the host cap, when free memory or
    commit falls below the floor, or when CPU stays over the cap for a minute.
  - After a reboot, workers come back a few per tick.
  - Builds take one host-wide FIFO pool, and a bare build in a worker is refused with the command that
    would run it.
- [ ] **AC6 — the view.** The host strip, per-worker CPU, memory and leftovers, one question list across
  hubs, a worker's processes with Clean up, its background tasks, attach in a new terminal tab, and
  open for the worktree, the PR and the issue.
- [ ] **AC7 — mail.**
  - It reaches a busy worker within one tool call and an idle one within 15 s, in any permission mode.
  - A stopped worker keeps its mail and is not restarted by it.
  - Nothing is delivered twice.
  - Mail never reads as the operator typing.
- [ ] **AC8 — the idle handoff.**
  - A worker that waits with a big context while its hub's tick is stale writes `handoff-g<N>.md` and
    reports HANDOFF READY.
  - It is skipped, with no model turn, for a parked or done worker, under a fresh hub tick, and on a
    cold cache.
  - A session that is neither hub nor worker writes its handoff outside every repository.
- [ ] **AC9 — field.** The three hubs resume on it. The first tick shows the host line and the badges,
  respawns are staggered, and a real TaskStop in a worker carries its cleanup line. A day later
  nothing is left behind.
- [ ] **AC10 — docs.** `docs/agent-hub.md` gains process hygiene, the host budget, the mailbox and the
  idle handoff, in house style (mermaid, tables), and its row in [`docs/README.md`](../README.md)
  says so.

## Open decisions

Decided by the operator on 2026-11-04:

| # | Question | Decision |
|---|---|---|
| D1 | Who builds it | One session directly, while the hubs are down after the 2026-11-03 reboot. |
| D2 | What a stopped worker's leftovers get | Killed, on Windows and in WSL, forked databases included, except what is kept or never-kill. The review refined this: the operator's own sessions get only the per-task cleanup. |
| D3 | Which hub-to-worker messages go through the mailbox | All of them, and the view's answers too. |
| D4 | Where the idle handoff runs | In every session. Hubs and workers use the hub's own format; other sessions write outside every repository. |

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
