# FR-154: The session tools keep a journal — two weeks of records show what each one does, and what to fix

**Issue:** [#4443](https://github.com/gjovanov/roomler-ai/issues/4443) · **Status:** claimed 2026-11-25. D1, D2 and D4
were decided by the operator the same day, and D3, D5 and D6 taken as recommended. P0 done 11:36:10Z. P1 is done except
for `bring-back.mjs`. P2 is done. P3's status-line half has been live since 12:27Z. Next are the module's events,
acct-sync's and crestore's. Builds on [FR-8](FR-8-claude-session-restore.md) (crestore),
[FR-136](FR-136-hub-process-hygiene.md) (hub-guard) and
[FR-152](FR-152-cold-hub-continues-from-handoff.md) (the cold rotation, whose `events.log` was the one record that logged
every decision). Like FR-152, it changes the operator's tooling under `~/.claude/` and no product code, so the design
names files and functions there rather than repository lines.

## Goal

The operator's session-continuity tools act many times a day without being asked:

- Cache Keeper: Handoff & Clear, Use Latest Handoff, Pack & Clear, and the hold on a prompt sent to a cold cache;
- handoff-guard: the handoff before `/exit`, `/clear` and `/resume`;
- hub-guard: the idle handoff and the cold rotation (FR-152);
- crestore: bringing sessions back after a restart (FR-8);
- acct-sync: packs and wakes across logins.

Most of what they decide leaves no record, and most sessions kept no transcript. So nobody can say how often each one
fired, what it saved, where it failed, or what it cost.

**Goal.** Every session keeps its transcript and its cost history, and every action or decision of these tools leaves
one line in one journal. One or two weeks of that is read, and the tools are fixed and improved from what it shows.

## Evidence

### The audit of 2026-11-24 (read-only)

| Source | What it kept before FR-154 |
|---|---|
| Claude Code transcripts | 45 of the 75 sessions active in six hours had none. A `claude` started from inside a Claude session inherits `CLAUDE_CODE_CHILD_SESSION`, and Claude Code then saves no transcript. |
| Cache Keeper's status line | The newest snapshot only (context, cache, dollars, the 5-hour and 7-day readings). It was overwritten every 30 s and deleted after 3 days. |
| Cache Keeper's module | Use Latest Handoff and the hold left no record. |
| handoff-guard | One state file per session, overwritten, and deleted after 7 days. |
| hub-guard | The idle handoff wrote nothing. The cold rotation (FR-152) logs every decision to `events.log`. |
| crestore, acct-sync | No record of runs, wakes or login switches. |

### A change to the user settings' `env` reaches running sessions (2026-11-25)

A key added to `env` in `~/.claude/settings.json` reached this session's own process within about 30 s. Claude Code
(2.1.322) applies the user settings' environment again when the file changes. A change to a `--settings <file>` of a
running session is not applied, so a lab session cannot predict what a user-settings change does to the running ones.

Claude Code decides at each write whether to save, and never backfills. So every running session that had saved
nothing began its transcript at the moment the flag arrived. The result is a **fragment**: its first entry with a
`parentUuid` names a parent the file does not hold, while a whole transcript's first one is `null`. There were 8 within
11 minutes. The set only shrinks: a session started later, and the conversation a `/clear` starts, are whole from their
first entry.

## Key design

### P0: a transcript for every session

`CLAUDE_CODE_FORCE_SESSION_PERSISTENCE=1` in the user settings' `env`. It overrides the `CLAUDE_CODE_CHILD_SESSION`
rule. `--no-session-persistence` still wins over it, and so does a test environment. It was proven first in the pty
harness: a session carrying the child marker saved its transcript with the flag and none without it.

### P1: a fragment is never taken for the whole conversation

| Where | How it tells | What it does with a fragment |
|---|---|---|
| acct-sync `handoff --kind clear` (Cache Keeper's Pack & Clear) | `partialStart()` reads the first 4 MiB | Refuses: "no session matches … as a whole transcript". Cache Keeper's `packOf()` reads that as "no transcript", so `packAndClear` packs the engine's own export of the conversation (`$.session.messages()`), which is whole. |
| acct-sync `handoff --kind restart\|switch` | `fragmentStart()`: `partialStart()`, then the whole file searched for the missing parent | The pack says where the saved part begins and that nothing before it was saved, and leads to the session's newest handoff. Its resume line says what a resume would load. |
| acct-sync `wake` | `fragmentStart()` | Routes to a handoff in every mode, with a `fragment` key in its JSON. It never routes to a resume. |
| crestore, `Restore-ClaudeSessions.ps1` | `Get-TranscriptFragment` (the same rule, `Test-ParentLater` for a file over 4 MiB) | Brings the session back from its own handoff-guard handoff, else from acct-sync's pack of the saved part (D5). Never resumes it: the picker refuses `r`, and the list marks it "partial transcript". |
| crestore, `Start-HandoffSession.ps1` | the wake file's `fragment` | Offers no `claude --resume`. |
| crestore, `bring-back.mjs` | (P1b) | Must refuse to resume a fragment and point to its handoff. In progress. |

**A fragment heals at its next compaction.** Claude Code then writes the part it never saved back into the file, after
the saved part, so the missing parent turns up later in the file. Packs, `wake` and crestore read a healed file as whole.
The clear refusal still reads only the first 4 MiB, so it can refuse a healed file whose parent lies further in. Pack &
Clear then packs the export, which holds the same conversation (D6).

### P2: one journal

The contract is `~/.claude/journal/README.md`, and every writer follows it. `~/.claude` there means `$CLAUDE_CONFIG_DIR`
when that is set, so a test points the journal at a scratch folder.

- **Where:** `~/.claude/journal/<YYYY-MM-DD, UTC>/`. A writer that runs in a session writes `<writer>-<sid8>.jsonl`. A
  script appends to `<tool>.jsonl`.
- **A line:** one JSON object. The base fields are `at`, `tool`, `ver`, `ev` and `sid`, and where they apply `next`
  (the session a clear or handoff started), `hub`, `outcome`, `why` and `tokens`. A reader skips a line that does not
  parse.
- **Never written:** an account address or id, a token or credential, the text of a prompt, reply, handoff or pack,
  file contents, a search pattern. A login appears only as the 8-hex fingerprint the tools already show.
- **Kept 30 days** (D3). Cache Keeper's status line deletes older day folders, at most once an hour, and only folders
  named as a date.
- **Off switch:** the file `~/.claude/journal/off`, or `CLAUDE_JOURNAL=off` in a writer's environment, stops every
  writer.
- **A writer never throws and never waits.** A failed write is dropped. Cache Keeper's module cannot append, because
  the engine's `$.fs` has no append and refuses a write over 4 MiB. So it keeps the day's lines in memory and rewrites
  its file whole. After a reload (any save in the plugin folder) it reads the day's file back first.

### P3, P4: the writers

| Tool | File | One line for | Phase | State |
|---|---|---|---|---|
| Cache Keeper's status line (`bin/statusline.mjs`) | `status-<sid8>.jsonl` | each change of the session's figures: context tokens, window, use, dollars, cache expiry and TTL, cold, model, and the 5-hour and 7-day readings | P3 | live since 2026-11-25 12:27Z (0.4.5) |
| Cache Keeper's module (`hooks/register.ts`, `hooks/core.mjs`) | `cache-keeper-<sid8>.jsonl` | Handoff & Clear, Use Latest Handoff and Pack & Clear (button or command, outcome, why, the next session, the context replaced); the cold-send hold and what the person did next; the 5-minute warning and the cold notice; a usage reading that moved a whole point; a 7-day reset time that changed, which is how a login switch shows | P3 | in progress (0.4.6) |
| acct-sync (`acct-sync.mjs`) | `acct-sync.jsonl` | each pack (kind, caller, outcome, the context replaced, the pack's size, transcript, fragment or export); each `wake` and `sync` decision and why; `plan`'s counts; each look-up of an old conversation (`show`, `render`: which view, never the pattern); a change of the login in use | P4 | in progress |
| crestore (`Restore-ClaudeSessions.ps1`, `Start-HandoffSession.ps1`, `hook.ps1`, `bring-back.mjs`) | `crestore.jsonl` | each run (mode, counts by route and reason, how long it took); each session started or skipped (route, fragment, why); each `-Prepare` answer; each start from a handoff; each session recorded, with the session it carries on | P4 | in progress |
| handoff-guard | `handoff-guard-<sid8>.jsonl` | each hold of `/exit`, `/clear` or `/resume`, the handoff turn it asked for, the release and why; the 90 % trigger | P4 | next |
| hub-guard, the idle part and its gate | `hub-guard-<sid8>.jsonl` | the idle handoff fired or skipped, and why | P4 | next. The cold part keeps its `events.log` and stays untouched until FR-152 closes. |

### P5: the report

A script reads the journal and the transcripts. Per tool it counts each event and outcome, and it lists the failures
with their reasons. It measures latency, for example from a Pack & Clear to the next session's first request, or from
a hold to the person's next step. It sets tokens avoided against tokens spent: a re-read a hold or a fresh start
avoided, the context a pack replaced, what a resume sent. It also follows the chains from each session to the one that
carried it on.
Day 7 is 2026-12-02 and day 14 is 2026-12-09, counted from P0. A writer installed later is read over its own days, and
the report says how many. The reports stay beside the journal (`~/.claude/journal/reports/`) and are not published.
Each defect they find becomes its own issue, written without session ids, fingerprints or account names.

### Kill switches

- **The journal:** `~/.claude/journal/off` or `CLAUDE_JOURNAL=off` stops every writer at once.
- **A writer:** the tool's previous version. Cache Keeper reverts to its previous commit in the installed work tree.
  acct-sync and crestore have no git, so each change leaves a backup folder beside the tool.
- **P0:** removing the key. A removal also reaches running sessions at once: a session with the child marker would
  stop saving mid-conversation. Its transcript would then end early while the session carries on, so acct-sync and
  crestore would take a stale transcript for the current one. Plan a removal; don't use it as a quick off switch.

## Phases

| Phase | Scope | Status | Kill switch |
|---|---|---|---|
| P0 | A transcript for every session: the flag in the user settings' `env`, proven first in the pty harness | **done 2026-11-25 11:36:10Z** (field log) | removing the key (above) |
| P1 | No tool takes a fragment for the whole conversation: acct-sync's clear refusal, packs and `wake`; crestore's restore and launcher | **done 2026-11-25**: the clear refusal at 11:45Z, the rest by 13:50Z, read on the live sessions (field log) | the backup folders |
| P1b | `bring-back.mjs` never resumes a fragment; acct-sync's `plan` and wake's "a resume sends" column know fragments | in progress | the backup folders |
| P2 | The journal's contract (`~/.claude/journal/README.md`): where, a line, never written, 30 days, the off switch, the writer rules | **done 2026-11-25** | the off switch |
| P3 | Cache Keeper: the status line's history (0.4.5), the module's events (0.4.6) | the status line **live since 12:27Z**; the module in progress | the off switch; the previous commit |
| P4 | acct-sync, crestore, handoff-guard, hub-guard's idle part and its gate | acct-sync and crestore in progress; the guards next | the off switch; the previous version |
| P5 | The report script | not started | — (it only reads) |
| P6 | The field: the day-7 and day-14 reports, and an issue for each defect they find | from 2026-12-02 | — |
| P7 | Docs: a section of `agent-hub.md` §12 on the journal, its writers, the report and the kill switches; the FR-8 and FR-152 cross-links | not started | — |

## Acceptance criteria

- [ ] **AC1** Every session started after P0 saves a transcript, with or without `CLAUDE_CODE_CHILD_SESSION`. Evidence:
  the sessions Cache Keeper saw start after P0, each with its transcript on disk.
- [ ] **AC2** A fragment is never packed or resumed as if it were the whole conversation: acct-sync's packs, `wake`
  and `plan`, crestore's restore and launcher, and `bring-back.mjs`. One cell each, on the live fragments.
- [ ] **AC3** Every session that made a request has its cost history in the journal.
- [ ] **AC4** Each action and decision of the tools in P3 and P4 leaves one journal line: one cell per kind.
- [ ] **AC5** No secret in the journal: no account address or id, no token, no prompt text, no file content. Evidence:
  a scan of every day folder.
- [ ] **AC6** The journal stays bounded: days past 30 are deleted, and its size per day is measured.
- [ ] **AC7** A journal write never fails or slows a tool: each tool's tests pass, and a failed write is dropped.
- [ ] **AC8** The report runs on the journal, and the day-7 and day-14 reports are written.
- [ ] **AC9** Each defect the reports find has its own issue.
- [ ] **AC10** Docs, written before the close (P7).

## Open decisions

| # | Decision | Recommendation |
|---|---|---|
| **D1** | Transcripts for every session (P0). | **Yes. Decided 2026-11-25 by the operator.** |
| **D2** | Publish the logging in the tools' public repositories. | **Not yet. Decided 2026-11-25 by the operator.** Local commits only; publishing waits for what the two weeks show. |
| D3 | How long the journal is kept. | 30 days, which covers both reports with room for a late read. Taken as recommended; D4 accepts its size. |
| **D4** | The status line writes a line for each changed figure: 2 to 3 a request, about 12 MB a day and about 360 MB at 30 days. Trim it? | **Keep every line, no trimming before the reports. Decided 2026-11-25 by the operator.** |
| D5 | A fragment with no handoff of its own: crestore brings it back from acct-sync's pack of the saved part, or lists it as one it cannot bring back. | The pack: the saved part is on disk, and the pack says what is missing. Taken as recommended 2026-11-25. |
| D6 | The clear refusal reads the first 4 MiB, while packs, `wake` and crestore search the whole file, so a large healed fragment is still refused at a clear. Search the whole file there too? | No. A refusal makes Pack & Clear pack the engine's export, which is whole by construction. A `/clear` drops the context, so the proven path wins over the richer one. Taken as recommended 2026-11-25. |

## Out of scope

- Product code: nothing in roomler changes.
- Fixing what the reports find. Each fix is its own issue (AC9).
- hub-guard's cold part, until FR-152 closes.
- The text of prompts, replies, handoffs and packs. The journal records what happened, never what was said.
- Publishing the tools' changes (D2).

## Docs

P7 writes a section of [`agent-hub.md`](../agent-hub.md) §12 before the close. It covers where the lines go, what a line
holds and never holds, the writers, the report, and the kill switches, with links from FR-8 and FR-152.
`agent-hub.md` already has its `docs/README.md` row.

## Field log

| Date | Read | Result |
|---|---|---|
| 2026-11-24 | The audit, read-only: the sessions active in six hours, and what each tool keeps | 45 of 75 sessions had no transcript. Only hub-guard's cold rotation kept a record of each decision (Evidence). |
| 2026-11-25 | P0's lab, before 11:36Z, in the pty harness with throwaway sessions | A session carrying `CLAUDE_CODE_CHILD_SESSION` saved its transcript with the flag in its settings. The same flag added to the `--settings` file of a session already running was not applied. The throwaway sessions' records were removed afterwards. |
| 2026-11-25 | P0 live, 11:36:10Z: the flag added to the user settings' `env` (backed up first) | The session that made the change had the flag in its own environment within about 30 s. Running sessions that had saved nothing began fragments: 8 by 11:47Z, 10 by 12:05Z. |
| 2026-11-25 | P1, 11:45Z: acct-sync's clear refusal | On a fragment, `handoff --kind clear` exits 1 with "no session matches … as a whole transcript", and Cache Keeper's `packOf()` reads that as no transcript, so Pack & Clear packs the export. On a whole transcript it packs as before. |
| 2026-11-25 | P2 and P3's status line: Cache Keeper 0.4.5, installed in one burst 12:26:57–12:27:06Z | 185 tests pass (180 before). At 12:39Z: 41 session files, 322 lines, none unparsable, each file's last line equal to its live snapshot. 45 snapshots were still written within 120 s. About 12 MB a day at this load, 2 to 3 lines a request (D4). |
| 2026-11-25 | The install's reload, 12:27Z | Reloading the module made Claude Code write one informational entry into every running session. 14 sessions that had saved nothing since 11:36Z began fragments within 30 s, 13 of them a 1 to 2 KB header and that one entry. Harmless. A plugin install is a moment that starts fragments. |
| 2026-11-25 | P1, acct-sync's packs and `wake`, crestore's restore and launcher (done by 13:50Z) | Outputs for whole transcripts were unchanged: 35 of 35 on 3 sessions after masking clock values. After the last change it was 31 of 35, and the other 4 differed only because one session's cache went cold between the runs. `wake` on the fragments went from 10 of 10 resumed to 10 of 10 handed off, in all three modes. crestore's new test has 26 checks. |
| 2026-11-25 | P1: a fragment heals at its next compaction | Three fragments compacted between 12:11Z and 13:50Z. Each time, Claude Code wrote the part it had never saved back into the file, after the saved part, ending at the session's root. Packs, `wake` and crestore now read a healed file as whole (D6). |
| 2026-11-25 | P1's read on the live sessions, about 14:00Z: 20 flagged sessions | `wake --mode resume`: 18 handed off with the `fragment` mark, and the 2 healed ones resumed. Restart packs of two fragments carry the line on where the saved part begins, and their resume line says what a resume would load. A clear on a healed file over 4 MiB is still refused (D6). crestore `-AsIfRestarted -List -From auto` on the real registry, with stand-ins first in `PATH`: 16 sessions from their own handoff and 1 from a pack, each marked partial transcript, and nothing started. crestore's suite: all passed, 17 sections, 475 checks. |

## Related

FR-8 (#780, crestore) · FR-136 (#3457, hub-guard) · FR-152 (#4290, the cold rotation; its `events.log` is the
model for this journal) · Cache Keeper, the hold for a person's cold prompt.
