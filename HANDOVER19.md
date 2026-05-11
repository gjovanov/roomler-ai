# HANDOVER19 — rc.22 ship + M3 elevated-switching plan + 2 open bugs

> Continuation of HANDOVER18. This session shipped rc.22 with always-PROGRAMDATA staging on Windows (Option B from HANDOVER18 Bug 1) and wrote the M3 elevated/user-app input plan (HANDOVER18 Bug 2). PC50045 verification pending.

## State at session end

- **Master HEAD**: rc.22 commits + tag `agent-v0.3.0-rc.22` pushed (assuming push step in P3 succeeds)
- **Workspace version**: `0.3.0-rc.22` (Cargo.toml + tauri.conf.json + config.rs `CURRENT_SCHEMA_VERSION`)
- **Tests**: 279 agent-lib tests green (was 279 on rc.21 — net +6 new rc.22 tests, -0 broken since cfg(test) keeps tests on legacy path)
- **Plan doc**: `docs/remote-control-m3-elevated-switching.md` — covers the bisect plan + 3 candidate fixes (A/B/C) for the admin-pwsh input regression

## What rc.22 shipped

### Code change (one file): `agents/roomler-agent/src/files.rs`

1. New `STAGE_IN_PROGRAMDATA: LazyLock<bool>` — process-static strategy flag. Default: `true` on Windows, `false` elsewhere or under `cfg(test)`. Env-var `ROOMLER_AGENT_STAGING_LEGACY_PER_DEST=1` reverts to legacy.
2. New `staging_root_windows()` returns `%PROGRAMDATA%\roomler\roomler-agent\staging\` (falls back to `C:\ProgramData` when `ProgramData` env unset).
3. `partial_dir_for(dest_dir, id)` now consults `STAGE_IN_PROGRAMDATA`:
   - Windows production → `staging_root_windows().join(id)` (NO `.roomler-partial` parent — flat layout)
   - Else / tests → `<dest_dir>/.roomler-partial/<id>` (rc.19 layout)
4. New `is_cross_volume_error(&io::Error)` — handles `ErrorKind::CrossesDevices` (Rust 1.85+) AND raw OS error codes (Windows 17, Linux 18) as belt-and-suspenders.
5. `end()` rename: try `tokio::fs::rename` first; on cross-volume error, fall back to `tokio::fs::copy` + `remove_file`. Tracing `info` line tells the field a cross-volume rename happened.
6. `sweep_orphans()` refactored: when PROGRAMDATA staging active, scans `staging_root_windows()` DIRECTLY (flat layout). Otherwise legacy `<Downloads>/.roomler-partial/`. Shared body extracted into `sweep_orphans_dir(&dir)`.
7. `resume_incoming` on-demand probe falls back to the correct canonical location based on strategy flag.

### Tests added (6 new in `agents/roomler-agent/src/files.rs::tests`)

- `rc22_stage_in_programdata_is_false_under_cfg_test` — pins test-mode legacy.
- `rc22_partial_dir_for_is_legacy_under_cfg_test` — verifies test layout.
- `rc22_is_cross_volume_error_recognises_kind` — `ErrorKind::CrossesDevices` mapping.
- `rc22_is_cross_volume_error_recognises_raw_os_error` — Windows 17 / Linux 18.
- `rc22_is_cross_volume_error_rejects_unrelated` — NotFound / PermissionDenied don't false-positive.
- `rc22_staging_root_windows_under_programdata` (Windows-only) — pins the path shape.
- `rc22_sweep_orphans_dir_handles_flat_layout` — verifies the new flat-sweep branch.

## What rc.22 does NOT fix

- **Admin-pwsh input regression (HANDOVER18 Bug 2)**: deferred. Plan doc at `docs/remote-control-m3-elevated-switching.md` covers the bisect + 3 candidate fix shapes (Change A = probe input desktop directly; Change B = bind every tokio worker; Change C = refine suppression policy under SystemContext).

## Verification needed on PC50045

Per the hypothesis-driven rc.22 design, verification is critical. Have the user:

1. **Install rc.22 perMachine MSI**: `roomler-agent-0.3.0-rc.22-perMachine-x86_64.msi` from `github.com/gjovanov/roomler-ai/releases/tag/agent-v0.3.0-rc.22`.
2. **Wait for SCM service to restart** (or restart manually: `Restart-Service RoomlerAgentService`).
3. **Test small file** (sanity, expected pass): drop a 100 KB text file. Should land in `C:\Users\e069019\Downloads\<name>.txt`.
4. **Test medium file** (5 MB - threshold probe): drop a 5 MB file. Note success/failure; if it fails, that narrows the cause significantly.
5. **Test large file** (14 MB .exe, the original repro): drop the same 14 MB .exe that failed under rc.21. If it succeeds, Option B was correct. If it fails, gather logs.
6. **Capture logs** during the failed upload:
   - Agent log: `C:\Windows\System32\config\systemprofile\AppData\Local\roomler\roomler-agent\data\logs\roomler-agent.log.<date>`
   - Look for: `files: cross-volume staging` (info), `files: ...sync_data failed` (debug), `files:error` (warn).
7. **Check staging dir leftover**: `ls C:\ProgramData\roomler\roomler-agent\staging\` — should be empty after a successful upload. If a stale `<id>/` dir remains, that's a sweep gap to investigate.

## If rc.22 STILL fails for large files

The hypothesis (PROGRAMDATA evades ESET per-chunk scanning) is wrong. Fall through to **Option D from HANDOVER18**: escalate to corporate IT to add the agent's install dir + staging dir to the ESET Remote Administrator exclusion policy. Template request:

> **From**: Goran Jovanov
> **To**: IT / ESET admin
> **Subject**: ESET exclusion request for roomler-agent
>
> Please add the following paths/processes to the ESET Real-Time Protection exclusion list for endpoints with roomler-agent installed:
>
> - **Process**: `C:\Program Files\roomler-agent\roomler-agent.exe`
> - **Path**: `C:\Program Files\roomler-agent\` (directory)
> - **Path**: `C:\ProgramData\roomler\roomler-agent\` (directory, includes staging)
>
> Justification: roomler-agent is a Roomler-deployed remote-control agent used for IT support. The Real-Time scanner currently intercepts per-chunk file-DC writes during large uploads, causing 14 MB+ transfers to fail with "reconnect budget exhausted". The agent is signed and digitally trusted.

## Next-session actions

### Immediate (user — on PC50045)

1. Install rc.22 perMachine MSI when CI publishes the release.
2. Run the 5-step verification flow above.
3. Report back: which file sizes succeed / fail + relevant log lines.

### Follow-up (next code session)

- **If Bug 1 closed by rc.22**: move to Bug 2. Bisect rc.7 → rc.21 on PC50045 per `docs/remote-control-m3-elevated-switching.md` §"Required pre-coding evidence". Implement Change A first (~30 LOC in `lock_state.rs`); ship rc.23.
- **If Bug 1 NOT closed by rc.22**: file the ESET exclusion request (Option D) AND proceed with Bug 2 in parallel. Bug 1 then becomes an operator-procedure issue, not a code issue.

## Files touched this session

- `agents/roomler-agent/src/files.rs` — major: PROGRAMDATA staging strategy + tests
- `agents/roomler-agent-tray/tauri.conf.json` — version bump
- `agents/roomler-agent/src/config.rs` — CURRENT_SCHEMA_VERSION bump
- `Cargo.toml` — workspace.version bump
- `CLAUDE.md` — rc.22 status block
- `docs/remote-control-m3-elevated-switching.md` — NEW plan doc (Bug 2)
- `HANDOVER19.md` — this file

## Open task carryover

- `#166 [pending] rc.19 P8: manual smoke on PC50045` — superseded by rc.22 verification above (covers the same kill-position scenarios + the new ESET hypothesis).

## How to pick up next session

1. Read this file first.
2. Read `docs/remote-control-m3-elevated-switching.md` for the Bug 2 plan.
3. User should have run the rc.22 verification on PC50045 by then; ask for the result.
4. Branch based on result:
   - rc.22 fixed Bug 1 → bisect Bug 2.
   - rc.22 didn't fix Bug 1 → file ESET exclusion request + bisect Bug 2 in parallel.

The user explicitly asked the session to PLAN M3 (the elevated/user-app switching), not implement. Don't code Bug 2 until the bisect identifies the breaking RC.
