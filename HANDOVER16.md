# HANDOVER16 — rc.19 agent-side shipped; browser side queued for next session

> Continuation of HANDOVER15 (which closed at rc.18 shipped + the
> 2026-05-11 field bug motivating rc.19). This session designed the
> rc.19 protocol via two independent plan-critique passes, then
> implemented the entire agent side (P0-P3 + P6) in 5 atomic commits.
> Browser side (P4-P5 + P7) is the next session's task.

## State at session end

- **Master HEAD**: `b23e9ea` (test(file-DC): rc.19 P6 — files:resume wire-format integration)
- **Pending phases**: P4-P5 (browser), P7 (UI polish), P8 (manual smoke on PC50045), P9 (tag agent-v0.3.0-rc.19)
- **No new agent tag yet** — agent code is on master but not released; ship after browser ready.
- **Web image on prod**: `v20260511-5ce2ab39e461` (unchanged from rc.18 cycle).
- **Lib tests**: 272 with default features. **Integration**: 13 file_dc tests. Clippy + fmt clean.

## What shipped this session (5 commits, agent-side)

| Commit | Phase | What |
|---|---|---|
| `0f109b1` | P0 | Wire format — `FilesIncoming::Resume { id, offset, sha256_prefix }`, `FilesOutgoing::Resumed { id, accepted_offset }`, caps `"resume"`, `PARTIAL_REGISTRY` + `ACTIVE_TRANSFERS` skeleton, 7 serde-lock tests |
| `9ba6f81` | P1 | Agent staging at `<dest_dir>/.roomler-partial/<id>/`, meta.json schema (`protocol_version=1`, filename, expected_size, dest_dir, reserved_final_path, created_at_unix, rel_path), `sync_data` per 1 MiB (B2 fix tuned for Windows Defender hosts), `sweep_orphans` at agent startup BEFORE WS connect, `begin()` rejects existing id (B1 fix), 5 unit tests |
| `a4be61f` | P2 | `FilesHandler::resume_incoming(id, offset)` — registry lookup → on-demand stat fallback → 256 KiB-aligned truncate (B3 fix) → re-open append → reinstall state. M4 fix: `unique_path()` re-runs at rename time. Cancel arm extended to upload-side (removes staging dir + registry entry on terminal browser failure). 5 unit tests. Poison-tolerant locking. |
| `ae9101e` | P3 | RAII `ActiveTransferGuard` increments `ACTIVE_TRANSFERS` on construct, decrements on Drop. Embedded as the last field of `IncomingTransfer`/`OutgoingTransfer` so it drops AFTER the file handle (kernel-flush guarantee). Updater `run_periodic` gains `decide_defer` gate: defer 1h-cycle when active > 0, force update after 7 consecutive defers (M3 fix). 4 updater tests pin the constants + logic. |
| `b23e9ea` | P6 | Wire-format integration test in `tests/file_dc.rs` — `files:resume { unknown_id }` → `files:error`. Browser auto-resume's fallback path relies on this. Full DC-close-then-resume integration test was prototyped but deferred — webrtc-rs loopback SCTP teardown races the second DC's chunk loop on Windows. Lib tests cover the same mechanics via direct `resume_incoming` calls. |

## rc.19 plan in `~/.claude/plans/floating-splashing-nebula.md`

Read first when picking up next session. Captures:
- Wire format envelopes (no per-request `resumable: bool` — cap-handshake is the only gate)
- 3 BLOCKER fixes (B1 sweep race, B2 sync_data durability, B3 truncation granularity) folded into the design
- 4 MAJOR fixes (M3 max-defer window, M4 reserved-path-stale, etc.)
- Backward compat: rc.18 browser + rc.19 agent works via legacy path; rc.19 browser + rc.18 agent falls back to fail-fast.

Two independent plan-critique passes found and resolved P0-P0-P0 issues in the original draft (caps wiring missing, innerPump captures stale channel, files:resumed reply silently dropped, files:cancel no-op on uploads, Transfers panel hidden during reconnect, sync_data perf claim off by 50-100x on Windows). All addressed in the final plan.

## Browser side (next session) — what's queued

### P4: caps wiring + bytesAcked tracking (~130 LOC, L risk)

- `ui/src/composables/useRemoteControl.ts` does NOT import `useAgentsStore` today. Plan adds: composable arg `agent: Ref<AgentDoc | null>` so `RemoteControl.vue` passes the current agent doc in. Composable computes `supportsResume = computed(() => agent.value?.capabilities?.files?.includes('resume') ?? false)`.
- Extend the inline upload variant of `RegistryEntry` (`useRemoteControl.ts:791-792`) to a named `UploadEntry` type with `bytesAcked: number`, `file: File`, `relPath?`, `destPath?`, `status: 'pending' | 'pending-resume' | 'settled'`.
- `files:progress` envelope handler updates `entry.bytesAcked = msg.bytes` for the matching id.
- 4 Vitest cases.

### P5: uploadOneResumable wrapper (~280 LOC, H risk)

- Refactor `uploadOne` (lines 2588-2720) into `innerPump(file, startOffset, id)` that re-reads `channels.files` on every invocation (P0-3 fix — the existing `const ch = channels.files` captures the DEAD channel after reconnect).
- Outer `uploadOneResumable(file, relPath, destPath, id = uuid())` retries up to 6 times:
  1. attempt 0 → send `files:begin`
  2. attempt 1..5 → await phase=='connected' → send `files:resume` → await `files:resumed` → pump from `accepted_offset`
  3. on terminal failure → send `files:cancel` (so agent cleans staging dir)
- `pendingResumePromises: Map<id, {resolve, reject, timer}>` — mirrors existing `pendingDirRequests` shape. P0-4 fix: `files:resumed` reply routes through THIS map, not `filesRegistry` (which the existing close handler clears).
- DC-close handler (lines 1882-1898) transitions entries to `status: 'pending-resume'` instead of settling-with-error when `supportsResume.value`.
- New `TransferStatus` variant `'reconnecting'`.
- 6 Vitest cases.

### P7: UI polish (~50 LOC, L risk)

- `RemoteControl.vue:332` — Transfers panel `v-if` currently `phase === 'connected'`. Change to `phase === 'connected' || phase === 'reconnecting'` so the operator sees the "Reconnecting N/6" status during the retry ladder (P1-5 fix).
- Status pill orange + label "Reconnecting N/6" for the new status variant.
- Pause the 10s auto-prune (line 833) while any Transfer is `'reconnecting'`.

### P8: Manual smoke on PC50045

Per the plan's Verification section:
- 35 MB xlsx + force `Stop-Process -Name roomler-agent` at 4/25/50/90%
- Auto-update mid-flight (`ROOMLER_AGENT_UPDATE_INTERVAL_H=0`) — observe defer log, post-completion fire
- Folder upload of 12 files, kill during 3rd
- 7 forced reconnects → 7th settles error + `files:cancel` cleans staging
- Set system clock +25h, restart → sweep_orphans cleans
- rc.18 browser + rc.19 agent compat
- rc.19 browser + rc.18 agent compat
- sync_data perf re-measure (re-time 35 MB upload, confirm < 10% overhead)

### P9: Tag `agent-v0.3.0-rc.19` + HANDOVER17

After P4-P8 land + smoke passes.

## Files the next session should re-read

1. `~/.claude/plans/floating-splashing-nebula.md` — the approved rc.19 plan (full design including all BLOCKER/MAJOR fixes folded in)
2. `~/.claude/projects/C--dev-gjovanov-roomler-ai/memory/project_rc19_resumable_transfers.md` — design decisions + user choices
3. `ui/src/composables/useRemoteControl.ts:791-820` (RegistryEntry shape), `:1882-1898` (DC close), `:2588-2720` (uploadOne pump), `:37` (reconnect ladder)
4. `ui/src/stores/agents.ts:22-36` (AgentCapabilities for the caps wiring)
5. `ui/src/views/remote/RemoteControl.vue:332` (Transfers panel v-if to fix)

## Field bug 2026-05-11 — status

Original: 35 MB xlsx upload from browser to PC50045 (Win11 perMachine) failed at 4 % (1,376,256/36,268,790 bytes), most likely from agent auto-update spawning msiexec mid-upload.

**Agent-side cure shipped this session** (master, not yet tagged):
- B2 + ACTIVE_TRANSFERS guard: auto-update CANNOT fire while any transfer is in flight (defers up to 7h, forces after 7 consecutive defers).
- B1 + B2 + B3: even if the agent is hard-killed mid-upload, the staging file survives, the partial registry rebuilds at startup, and resume continues from the last 1 MiB-aligned durable offset.
- M4: operator file ops during upload don't clobber the final.

**Browser-side cure pending (P4-P5)**: until the browser implements the auto-resume loop, an rc.19 browser still sees the existing "files channel closed" error and the operator has to manually re-drop the file. The agent's staging dir persists (cleaned by browser-driven `files:cancel` on terminal failure, OR the 24h orphan sweep). So pre-P5 browsers don't BENEFIT from rc.19, but rc.18 browsers don't BREAK either.

Field-test plan: after P4-P9 ship rc.19 in full, repeat the 35 MB xlsx upload + force kill on PC50045 and verify SHA match end-to-end.

## How to pick up next session

1. `cd C:\dev\gjovanov\roomler-ai && git log --oneline -8` — verify the 5 agent-side rc.19 commits + 1 handover update are present.
2. Read `~/.claude/plans/floating-splashing-nebula.md` (still the approved plan; no `/plan` re-run needed unless scope changes).
3. Start P4 by adding the composable arg + agent doc plumbing.
4. P5 is the hard one — refactor `uploadOne` into an `innerPump` and wrap it. Vitest is the safety net.
5. Ship P4-P7 in 1-2 commits each, run `bun run test:unit` + `vue-tsc --noEmit` per commit.
6. Smoke on PC50045 (P8).
7. Tag agent-v0.3.0-rc.19.
