---
name: autopilot
description: Drive the open FR backlog toward closure — admit issues by label, rank them closest-to-done first, work one card at a time in its own worktree through implementation, CI, docs and REAL field verification, and park at Ready-to-close for the operator. Tracks every card as JSON in the private roomler-ai-docs repo and renders a kanban board from it. Use when working the backlog rather than a single named issue, when asking "what should I pick up next", when you want the board refreshed, or when you want to know what is waiting on the operator personally.
---

# Autopilot — the FR backlog, worked toward closure

44 FR issues are open. Most are not stalled for want of code; they are stalled because
nobody has carried the last mile — the field read, the docs, the tick. This drives that
last mile, one card at a time, and **stops one step short of closing anything.**

## The key insight

**"Closure" in this repo is not a merged PR.** `CLAUDE.md` says it in three places and the
FR workflow enforces it: *CI green ≠ done*. An FR closes when its acceptance criteria are
**field-verified** and its docs exist. So an agent optimised for closure against a CI signal
would manufacture exactly the lie this codebase is built to prevent.

The resolution is a split nobody had written down before: **every acceptance criterion is
either agent-reachable or operator-only.**

- **agent** — a measurement, a test, a fleet read via `roomler exec`, a `vmtest` /
  `profiletest` / `tunnel-fleet-test` cell, a Chrome-driven RC session, a doc.
- **operator** — a human's perception or a human's decision. FR-77 is open *only for the
  operator's Notepad++ scroll*. FR-74's remainder is *the operator's cap call*. FR-1 was
  verified by *"Rozalina works nicely"*. No agent reaches those, ever.

A card is **Ready to close** when every *agent* criterion is ticked with linked evidence.
The board's top section then answers the question that exists nowhere else today:
**what, across 44 FRs, is waiting on you personally?**

⚠️ **Measuring a number is `agent`. Deciding what the number should be is `operator`.**
That line is the whole classification.

## Where it runs

- **Driver**: the dev box, interactive. Deliberately **not** a workflow lane — the sibling
  `daily-health-check.yml` only *reports*; this one writes code and opens PRs, and those
  deserve different leashes until it has field hours.
- **Cards + board**: `roomler-ai-docs` (private) `kanban/` — `state/<FR-n>.json` per card,
  `BOARD.md` generated. Cloned at `C:/dev/gjovanov/roomler-ai-docs`.
- **Engineering truth stays public**: the GitHub issue and `docs/fr/FR-n-*.md` remain
  authoritative for what was built and verified. The card holds only *run* state — branch,
  PR, attempts, blocker, next step — which has no public home. The docs repo's own rule:
  a public document must stand alone.
- **Tool**: `.claude/skills/autopilot/autopilot.mjs`, zero dependencies. Node, because this
  box has **no python3 and no jq on the Git-Bash PATH** (measured 2026-09-08); `gh --jq`
  covers GitHub reads.

## Driving it

```bash
cd /c/dev/gjovanov/roomler-ai
git fetch origin '+refs/heads/*:refs/remotes/origin/*'   # origin/master goes stale otherwise

node .claude/skills/autopilot/autopilot.mjs scan    # ledger + specs + issues + PRs -> cards
node .claude/skills/autopilot/autopilot.mjs next    # what to pick up, closest-to-done first
node .claude/skills/autopilot/autopilot.mjs board   # regenerate BOARD.md

cd /c/dev/gjovanov/roomler-ai-docs && git add kanban && git commit -m "kanban: <what moved>" && git push
```

`scan` is idempotent and safe to run any time. It **never** writes to the public repo,
never edits a spec, never touches an issue.

## The phases a card goes through

| # | phase | what happens | leaves the card in |
|---|---|---|---|
| 0 | **orient** | fetch master, `scan`, read the board | — |
| 1 | **triage** | admit by `autopilot` label; sort anything needing a human call | `admitted` / `judgement` |
| 2 | **pick** | `next` — highest ticked-AC fraction, `priority` overrides | `in_progress` |
| 3 | **build** | own worktree `ap-<issue>`, implement the next phase, CI green | `in_progress` |
| 4 | **docs** | mermaid + a `docs/README.md` row, **in the same PR** | `pr_open` |
| 5 | **verify** | the real field read — the skill that covers this pillar | `field` |
| 6 | **park** | tick agent ACs with evidence links, update card, push board | `ready` |

## Safety rails (do not violate)

1. **Never close an issue. Never merge a PR.** The card parks at `ready`; the operator closes.
2. **Never tick an acceptance criterion on CI evidence.** A green lane is not a field read.
   If the only evidence is CI, the AC stays open and the card says so.
3. **Never claim an FR number.** Write the proposal on the card and in the issue; the
   operator claims. The ledger's arbiter is a push race on one shared table with **six
   recorded collisions** — two workers racing it reproduces that at machine speed.
4. **Never adopt a bare branch.** An open PR is adoptable (its diff is reviewable, its
   intent stated). A branch is not. This is the #1144 shape: a merged, field-verified fix
   silently reverted, green CI, no conflict.
5. **Never delete a worktree or branch it did not create.** 62 worktrees and 1043 local
   branches predate this skill. Report them; prune nothing.
6. **Pillar work runs at the top tier.** Anything touching remote desktop, the overlay or
   WebRTC is `CLAUDE.md` §3 work. Budget decides *whether a worker starts and when it
   stops* — never *which model runs it*. A worker that exhausts its budget parks the card
   saying so, visibly. It never finishes on a weaker model.
7. **One worker per card, its own worktree, 2–3 in flight.** The card is the claim.
8. **`BOARD.md` is generated.** A hand edit is overwritten without warning.

## Traps

1. **A queue that silently drops a row looks exactly like a queue with nothing to do.**
   `scan` warns on a ledger row whose issue is `#TBD` and on an open FR issue with no
   ledger row. Its first run found a real **FR-80 collision** that way: master holds
   FR-80 for #1532 while a local branch held it for an unpublished mesh-stress spec.
2. **A verdict file written only at finish makes a stale run look current.** `profiletest`
   carries this warning in writing about its own `LATEST`. Cards are therefore written
   incrementally and carry `run.heartbeat`; a card with a heartbeat and no PR is a
   *crashed* worker, not a finished one.
3. **Phase tables are not parseable and nothing may depend on them.** Only 41 of 79 specs
   have `## Phases`, the tables under it use 15+ distinct header shapes, and FR-70 has
   prose instead. Acceptance criteria are the spine — all 43 open cards parsed theirs.
   ⚠️ Do **not** normalise the specs to suit the parser: they are the record of decisions.
4. **`git fetch origin master` can leave `origin/master` stale.** Use the explicit refspec
   above before any assertion about master, including the collision check.
5. **`gh` writes pass through `.claude/hooks/gh-account-guard.sh`**, and it matches the
   payload as *text* — a command that merely mentions `gh issue create` in a heredoc trips
   it. Reads always pass.
6. **One issue per condition, never one per occurrence.** If autopilot ever files an issue,
   copy `release-agent.yml`'s `file-or-update-alert` (search `in:title` → comment or
   create, paired with close-on-green), never `vmtest.sh`'s unconditional create — that is
   what produced 29 open `vmtest:` issues nobody can close.

## Adding coverage

- **A new field-verification route**: map the pillar to its skill in phase 5. Remote
  desktop → a Chrome-driven session; install/enrollment → `vmtest`; profiles →
  `profiletest`; tunnels/overlay → `tunnel-fleet-test`; server → `roomler exec` reads.
- **A new card kind** (a plain bug, not an FR): `kind: "bug"`, no spec, ACs authored on the
  card from the issue body. Everything downstream already treats `acs` as the spine.
- **Re-classifying an AC**: edit `verify` on the card. `scan` preserves any value that is
  not `unclassified`, matching on AC id first and text second — so a reworded criterion
  keeps its judgement only when its id is stable.
