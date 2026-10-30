# FR-131: Hub — one coordinator session runs the backlog through parallel worker sessions (`/hub` skill + hub view)

**Status: P1 and P2 shipped 2026-10-30. P3 (the first real backlog run) in progress. P4 (the docs) in review.**
P1 is the skill, the worker protocol and the helper, with the mechanics measured live. P2 is the hub view mod,
`hub-view` 0.2.0. The engineering doc is [`docs/agent-hub.md`](../agent-hub.md).

## Problem / Goal

The FR backlog is worked one card at a time (the `autopilot` skill), with two or three workers sent out by
hand as in-process subagents. A subagent can't be entered by the operator. It dies with its parent's
`/clear`. It reports only when it finishes. And nobody sees its context filling up or its prompt cache
expiring. Three things have been measured, each the cost of running workers that way:

- a worker silently continued on a weaker model;
- two workers halted on a usage limit;
- a worker improvised live network manipulation on the dev box because "prove it in the field" was left
  open.

**Goal:** one coordinator session, the **hub**, delegates FRs, bugs, specs and research to **worker
sessions** that run in parallel. Each worker has its own session and worktree, which the operator can
enter and leave. All of them are tracked on one board. Each worker's context is rotated before it
degrades. And every claim a worker makes is verified on GitHub, CI or the fleet before it counts.

## Key design

```mermaid
flowchart LR
  OP([operator]) -- "/hub · answers · Pause, Queue" --> HUB[hub session<br/>/hub skill]
  HUB -- "claude --bg in a hub-* worktree<br/>brief · model · effort · same permission mode" --> W1[worker W-1]
  HUB --> W2[worker W-2]
  W1 -- "SendMessage: STARTED, QUESTION, PARKED…" --> HUB
  HUB -- "ANSWER, PUSH, HANDOFF, PAUSE…" --> W1
  W2 -. "sub-hub (depth 2)" .-> W21[worker W-2.1]
  HUB -- "board.md every tick" --> ISSUE[(hub issue<br/>private repo)]
  W1 -- "Step log, PR" --> FR[(FR issue · PR<br/>public repo)]
  T[telemetry: claude agents --json ·<br/>Cache Keeper live snapshot ·<br/>transcript · task list] --> HUB
```

| Part | Where | What it does |
|---|---|---|
| `/hub` skill | `~/.claude/skills/hub/SKILL.md` | The coordinator. It defines Start, Tick, the signal table, model and effort tiers (§3 scope pinned to the top tier at max), dispatch, rotation (handoff, then the next generation), verification on the surface, operator escalation with exact commands, pause and resume, FR claims (the hub is the only claimant) and nesting. |
| Worker protocol | `~/.claude/skills/hub/worker.md` | Start (orient on the surface, plan, report), the loop, the message verbs both ways (their single definition), questions, handoff, park, sub-hub, and the `status.json` schema. |
| Templates | `~/.claude/skills/hub/templates.md` | The hub issue, the brief, the worker's progress comment, and the FR, task and sub-issue shapes. |
| Helper | `~/.claude/skills/hub/hub.mjs` | No dependencies. `init`, `add`, `spawn` (including `--rotate [--from-status]`), `stop`, `respawn`, `msg`, `status` (the board plus recommendations), `sync` (reading the hub issue's controls and queue) and `attach`. |
| Hub view mod (P2) | `~/.claude/skills/hub-view/`, a function-hooks plugin (`hub-view` 0.2.0) | `/hub` with no arguments lists every worker of every hub, active and inactive, with total, done and in-progress task counts. A worker can be entered (its task list and its whole conversation), answered, activated or deactivated, and left again for the hub view. |
| State | `~/.claude/hub/<id>/` | `state.json` (the registry, written only by the hub), `status.json`, `board.md` and `board.txt` (generated), `events.log`, and `workers/<W>/` (the brief, `status.json` written by the worker, and the handoffs). |

## Evidence — the mechanics the design rests on

All measured 2026-10-30 on Claude Code 2.1.280, except the last row, with throwaway workers that were
removed afterwards.

| Measured | Consequence in the design |
|---|---|
| `--effort low` recorded `effort: max` in the transcript: the project's `env.CLAUDE_CODE_EFFORT_LEVEL=max` beats the flag. `--settings '{"env":{…}}'` recorded `high`. | `spawn` sets the effort through `--settings` env, and `status` reads the effort each turn actually used. |
| A background session in another permission mode held the hub's message ("permission mode class doesn't match") and let it expire. With the modes matched, messages flowed both ways. | Every worker starts in the hub's own mode, read from the hub's transcript. `spawn` refuses while the mode is unknown. |
| `claude --bg --resume <id>` on a stopped, registered session started a copy under a new id that inherited the name. `claude respawn <id>` resumed it in place. | Pause is `claude stop`, resume is `claude respawn`. |
| A session started inside a worktree shares auto-memory, CLAUDE.md, the rules, the project env and the hooks (the gh-account guard ran). It lacks `settings.local.json`, so `GH_CONFIG_DIR` is unset, and it lacks `permissions.defaultMode` and the untracked skills. | `spawn` forwards `GH_CONFIG_DIR` and a per-worktree `CARGO_TARGET_DIR`. Briefs pass untracked skills by absolute path. |
| `claude agents --json`: `state: blocked` is the normal label of an idle job; a real prompt reads `status: "waiting"` with `waitingFor`. A `/clear` inside a job gives it a new session id under the same job id. | Liveness follows the job id, and a person is needed only on `status: waiting`. |
| A `<synthetic>` reply is a limit hit only with `error: "rate_limit"` ("You've hit your session limit · resets …"). After a stop or respawn the session writes "No response requested.", which is not a limit hit. | `LIMIT` fires on real limit hits only and quotes the reset. `API_ERROR` is a separate signal. |
| On 2.1.280, `claude-sonnet-5-5` served, but as an unrecognised model with a 200k window and unknown pricing. On **2.1.314** it is recognised with a 1M window and list pricing, and the `sonnet` alias resolves to it. | Claude Code was moved to 2.1.314 after the operator's three function-hooks mods passed their own suites on it (62, 67 and 63 tests). |

## Phases

| Phase | What | Status | Kill switch |
|---|---|---|---|
| P1 | Skill, worker protocol, templates and helper. Mechanics measured with five throwaway workers. An independent review found 12 defects, all fixed and retested. | shipped 2026-10-30 | Don't invoke `/hub`; delete `~/.claude/skills/hub/`. |
| P1b | Claude Code 2.1.280 → 2.1.314, so that Sonnet 5.5 is first-class | done 2026-10-30 | Rename the kept 2.1.280 binary back. |
| P2 | Hub view mod: list, counts, enter (tasks and conversation), answer, activate and deactivate, back. `hub-view` 0.2.0, 35 tests, shown live on 2.1.314 ([Step log](https://github.com/gjovanov/roomler-ai/issues/3127#issuecomment-6515103295)). | shipped 2026-10-30 | Remove the plugin's folder. |
| P3 | First real backlog run, at least two workers in parallel on real FRs | in progress 2026-10-30 | ⏸ Pause in the hub issue. |
| P4 | Docs: `docs/agent-hub.md` in house style, plus a `docs/README.md` row | in review, PR #PRNUM | Revert the PR; the tooling does not read the doc. |

## Acceptance criteria

- [x] **AC1 — spawn.** A worker started by `hub.mjs spawn` runs on the tier's model and effort, in the hub's
  permission mode, with `GH_CONFIG_DIR` set and a `CARGO_TARGET_DIR` outside its worktree. Evidence (field
  log 2026-10-30): the worker reported `gh=set cargo=C:/dev/_targets/<worktree> eff=high`, and its
  transcript recorded `high` on Sonnet in `bypassPermissions`.
- [x] **AC2 — messages.** The hub and a worker exchange messages both ways when their modes match. A
  mismatched mode is shown to hold the message.
- [x] **AC3 — rotation.** `spawn --rotate` refuses without a handoff. `--from-status` writes an UNVERIFIED
  handoff, stops gN, and starts gN+1, whose first message points at that handoff.
- [x] **AC4 — pause and resume.** `stop` followed by `respawn` brings the same job back, idle, with nothing
  replayed.
- [x] **AC5 — signals.** On real transcripts, `LIMIT` fires on a real limit hit and quotes its reset, a
  "No response requested." reply raises nothing, and a model switch is flagged as drift.
- [x] **AC6 — hub view.** `/hub` with no arguments lists every worker of every hub, active and inactive,
  each with total, done and in-progress task counts that match its `status.json` plan. Evidence: shown
  live on 2.1.314 on 2026-10-30, with no model turn. Both hubs were listed, and each worker's counts
  equalled its `status.json` plan: 12 · 9 · 1 · 2, 3 · 1 · 1 · 1 and 4 · 1 · 1 · 2
  ([Step log](https://github.com/gjovanov/roomler-ai/issues/3127#issuecomment-6515103295)).
- [x] **AC7 — enter.** Entering a worker shows its task list and its whole conversation, scrollable, and
  "back" returns to the hub view. Evidence: shown live, including a multi-megabyte conversation paged
  from its end with `⇡ Older` above it, and the first generation opened read-only. Back, Esc and Older
  are key presses, which UI tests on both terminal and desktop exercise
  ([Step log](https://github.com/gjovanov/roomler-ai/issues/3127#issuecomment-6515103295)).
- [x] **AC8 — answer.** An answer typed in the view reaches the worker, and the worker continues with it,
  as shown in its transcript. Evidence: `/hub-view answer` read `✔ received · route a`, and the worker's
  transcript shows the answer arrive from the view, then the worker mark q1 answered and reply
  `applied: ANSWER q1: …`
  ([Step log](https://github.com/gjovanov/roomler-ai/issues/3127#issuecomment-6515103295)).
- [x] **AC9 — activate and deactivate.** `respawn` and `stop` work from the view, and the hub's state
  follows. Evidence: a deactivate left `state.json` at `paused` with the stop stamped, and an activate
  respawned the same job, not a fork, with `state.json` back at `running` and the RESUME acknowledged
  ([Step log](https://github.com/gjovanov/roomler-ai/issues/3127#issuecomment-6515103295)).
- [ ] **AC10 — real run.** At least two workers in parallel each park one phase of a real FR with PRs the
  hub has verified. The board and the hub issue track them, and at least one rotation happens on a
  context or cache signal.
- [x] **AC11 — docs.** `docs/agent-hub.md` in house style (mermaid, tables) and a `docs/README.md` row.
  Evidence: [`docs/agent-hub.md`](../agent-hub.md), with four mermaid diagrams (the hub and its spokes,
  the tick loop, a worker's states, the message sequence) and tables for the pieces, state, verbs,
  signals, tiers, rotation, the view and the traps; and its row in [`docs/README.md`](../README.md),
  under 🧭 How the work is run, in the table and the map (PR #PRNUM).

## Open decisions

All three were resolved on 2026-10-30.

- **The command.** `/hub` with no arguments opens the view; `/hub <args>` stays with the skill.
  **Resolved 2026-10-30, as written.** P2 built it that way, and `/hub tick <id>` still reached the
  skill in the live check.
- **The answer route.** If the function-hooks API can send a cross-session message, the view sends the
  answer directly. Otherwise it relays through the hub session (`W-n: <answer>`). **Resolved
  2026-10-30 by this rule: directly.** P2 found that the API has a cross-session send
  (`$.session.send`), so the view sends the answer to the worker itself (route a). The relay through
  the hub (route b) is used only when the view runs in the hub session and its permission mode
  differs from the worker's; otherwise the view copies the attach command and sends nothing (route c).
  The hub recorded the resolution in the issue the same day.
- **Where the tooling lives.** The skill and the mod could move into this repo (tracked, like `autopilot`)
  or into a repo of their own (like Cache Keeper). **Decided 2026-10-30 by the operator: it stays
  local for now.** The skill and the view live in the user's Claude skills folder, each with a local
  source history; neither is in this repo.

## Out of scope

- Agent teams (they can't nest and don't survive `/resume`) and in-process subagents as workers (they
  can't be entered and die with their parent's `/clear`).
- The hub merging, closing, tagging, promoting or deploying: those stay with the operator.
- Cloud and remote-machine workers.

## Field log

- **2026-10-30 — P1.** Five throwaway workers (Sonnet), all removed afterwards; every mechanic above was
  measured on them.
  - **Dead hypothesis:** a stopped job resumes with `--bg --resume`. It forks a copy.
  - **Dead hypothesis:** `state: blocked` means a worker needs a person. It is the normal idle label.
  - **Dead hypothesis:** any `<synthetic>` reply is a limit hit. "No response requested." follows every
    respawn.
- **2026-10-30 — P1b.** 2.1.314 was staged in a separate folder, probed (Sonnet 5.5 served, a 1M window,
  list pricing, `sonnet` resolving to it), and the operator's mods' suites passed on it. The global binary
  was then swapped by rename, so the running sessions stayed on 2.1.280.
- **2026-10-30 — P2.** One worker built `hub-view` 0.2.0 across three generations, under the first hub.
  g1 handed off to g2 at 35% context, after reading the function-hooks API declarations had grown it by
  about 80k tokens in nine minutes. g2 handed off to g3 across a budget hold: the account's 5-hour
  window went from 55 to 89% in about 50 minutes. The hub had g2 hand off at 85%, stopped it, and
  started g3 after the window reset, because the hub starts no session under the hold and a rotation
  starts one. By checking claims on the surface, the hub caught two gaps before the park: AC7's paging
  had not been shown live, and the helper's `self` read the permission mode as null from a 2.1.280
  transcript. Both were closed. The plugin has 35 tests, every UI test on both terminal and desktop,
  and AC6–AC9 were then shown live on 2.1.314 with two throwaway workers, which were removed afterwards
  ([Step log](https://github.com/gjovanov/roomler-ai/issues/3127#issuecomment-6515103295)).
  - **Dead hypothesis:** a send whose `isDelivered` is true has reached the worker. It means queued: a
    session in another permission mode holds the message and lets it expire. The view counts an
    answer as received only once it shows in the worker's transcript.
  - **Dead hypothesis:** the view can read a worker's transcript whole. `$.fs.read` refuses a file over
    4 MiB, and worker transcripts are bigger, so `hub.mjs convo` pages one back from its end by byte
    offset.
  - **Dead hypothesis:** the hub relay can submit from the `/hub` hook. The engine refuses a submit from
    a `command.run` hook that holds the turn, so route b submits from a timer.
