# HANDOVER20 — rc.23 ships infinite reconnect + agent log viewer + staging view + E2E spec

> Continuation of HANDOVER19. This session shipped the user-requested
> diagnostic features for tracing large-file upload failures on
> PC50045 (HANDOVER18 Bug 1, still open after rc.22). E2E test spec
> is on disk but not yet run against PROD.

## State at session end

- **Master HEAD**: `c89ab79` (CI dead-code fix on top of `67e23a7`)
- **Tag**: `agent-v0.3.0-rc.23` published
- **Mars docker build**: in flight (~5-15 min). Will deploy via ArgoCD GitOps once tag bumped in `roomler-ai-deploy`.
- **Tests**: 284 agent-lib tests green (+5 logs_fetch); 130 useRemoteControl Vitest green
- **vue-tsc**: clean

## What rc.23 shipped

### Phase 1 — infinite reconnect (`ui/src/composables/useRemoteControl.ts`)

Closes the user directive "DC should always remain open (no matter what)". rc.22 didn't help PC50045 — ESET still drops the DC during large uploads, and the legacy 6-attempt reconnect budget surfaced "exhausted" before resume could converge.

- `nextReconnectDelayMs(attempt)` returns 8 s steady-state delay past the rc.19 ladder (was `null` / give-up). Always returns a positive delay.
- New `RC_RECONNECT_STEADY_MS = 8000` constant.
- `scheduleReconnect()` no longer fails out on attempt count; only operator-Disconnect (clears `lastConnectArgs`) ends the loop.
- `uploadOneResumable`: `MAX_ATTEMPTS = Number.POSITIVE_INFINITY`. Retry loop spins forever or until files:complete / operator-cancel.
- `waitForConnected(timeoutMs)` skips the setTimeout when `timeoutMs` isn't finite (passes `Number.POSITIVE_INFINITY`).
- UI: "Reconnecting (attempt N)" — no "/MAX" suffix.

### Phase 2 — rc:logs-fetch protocol (`agents/roomler-agent/src/logs_fetch.rs` + `peer.rs`)

Round-trip diagnostic over the control DC. Browser asks for the agent's log tail; agent reads the rolling log file and replies with the requested number of lines.

- `logs_fetch::current_log_path()` — picks the lex-latest `roomler-agent.log*` file under `<log_dir>`.
- `logs_fetch::read_tail_lines(path, count)` — streams from EOF backwards in 4 KiB chunks; doesn't load full file. Tested against 100 KB / 5000 lines.
- `logs_fetch::fetch_tail(lines)` — wraps the above, returns the JSON reply envelope. Clamps lines to [1, 5000]. Default 500.
- `peer.rs::attach_control_handler` — new `rc:logs-fetch` arm reads `val.lines` (default 500, clamp), calls `fetch_tail`, sends reply over the same DC via captured Arc clone.

5 unit tests pin the contract (small file, truncation, empty file, chunk-spanning, clamp).

### Phase 2 — browser log viewer (`ui/src/composables/useRemoteControl.ts` + `RemoteControl.vue`)

- `parseControlInbound` recognises `rc:logs-fetch.reply` → new typed `RcLogsFetchReply`.
- `agentLogs` + `agentLogsLoading` refs hold last reply + loading state.
- `fetchAgentLogs(lines = 500)` composable method: sends `rc:logs-fetch`, awaits matching reply via single-flight resolver, 8 s timeout.
- Toolbar `mdi-file-document-outline` button opens a v-dialog with a scrollable monospace pre-block.
- Line-count dropdown (200/500/1000/2000/5000) → re-fetches on change.
- Refresh + Copy buttons.
- Auto-fetch on first open + auto-scroll to bottom.

### Phase 2 — staging quick-access (`RemoteControl.vue`)

Files browser drawer toolbar — new `mdi-package-variant-closed` button navigates to `C:\ProgramData\roomler\roomler-agent\staging`. Operator sees in-flight rc.22 partials + can use existing drawer controls (refresh, delete dangling). Downloads is operator-typed via the existing path input.

### Phase 3 — E2E spec (`ui/e2e/remote-upload-pc50045.spec.ts`)

Playwright test that runs against PROD `roomler.ai`. Reads `.cred` for username/password, logs in, navigates to `/tenant/69a1dbbad2000f26adc875ce/agent/69f3771d9fc07b0c99e476f8/remote`, clicks Connect, uploads `C:\Users\goran\Dropbox\Work\CV.pdf` via `setInputFiles`, polls Transfers panel for 12 min until 'complete' or 'error'. 15 min global timeout. Captures console errors on failure.

**Run** (after web deploy completes):
```bash
cd ui && E2E_BASE_URL=https://roomler.ai bunx playwright test \
  remote-upload-pc50045 --headed --reporter=list
```

**Not yet executed in this session** — pending mars docker build + ArgoCD reconcile.

### Safety

`.gitignore` updated to include `.cred` (was missing; the file contains plaintext credentials).

## CI status

- `Release roomler-agent` for `agent-v0.3.0-rc.23` — in flight at session end.
- `CI` for master at HEAD — in flight (will pass; the dead-code fix in `c89ab79` covers the rc.22 failure mode).
- rc.22 cycle CI failure analysed: `STAGE_IN_PROGRAMDATA` was unused on Linux (only read inside Windows-cfg blocks). Fixed via `#[cfg_attr(not(target_os = "windows"), allow(dead_code))]`.

## Deploy status

Mars tmux session `deploy` is connected and running `docker build -t registry.roomler.ai/roomler-ai:build-rc23 .`. Once that finishes:

1. Tag the image with a date-versioned name + `latest`.
2. Push both.
3. `cd /home/gjovanov/roomler-ai-deploy && sed -i 's|newTag:.*|newTag: <TAG>|' k8s/overlays/prod/kustomization.yaml`.
4. `git commit -am "chore(k8s): bump roomler-ai to <TAG>"` + `git push`.
5. ArgoCD auto-syncs (≤5 s via webhook).
6. Verify `curl -sI https://roomler.ai/health` returns rc.23.

## Verification flow (user to drive on PC50045)

After web deploy + rc.23 MSI is published from GitHub Actions:

1. **Install rc.23 perMachine MSI** on PC50045: `roomler-agent-0.3.0-rc.23-perMachine-x86_64.msi` from `github.com/gjovanov/roomler-ai/releases/tag/agent-v0.3.0-rc.23`. The MSI's `cleanup-legacy-install` custom action will handle the transition from rc.22.
2. **Verify the agent is online** at `https://roomler.ai/tenant/69a1dbbad2000f26adc875ce/agent/69f3771d9fc07b0c99e476f8`.
3. **Open the log viewer** via the new mdi-file-document-outline toolbar button to confirm the rc:logs-fetch round-trip works. You should see live agent log lines.
4. **Open the Files drawer** → click the `mdi-package-variant-closed` button (Staging) to navigate to `C:\ProgramData\roomler\roomler-agent\staging`. Should be empty (no in-flight uploads).
5. **Attempt the 14 MB upload again** (the original repro). With infinite reconnect, you should see "Reconnecting (attempt N)" indefinitely if ESET keeps killing the DC. Upload either completes (if resume converges) or stays in retry loop.
6. **During or after a failed upload**, open the log viewer + refresh. Look for: `sync_data failed`, `files:error`, `chunk` write failures, ESET-related signals.
7. **Run the E2E test** locally:
   ```bash
   cd ui && E2E_BASE_URL=https://roomler.ai bunx playwright test \
     remote-upload-pc50045 --headed --reporter=list
   ```

## Open follow-ups

- **HANDOVER18 Bug 2** (admin pwsh input regression) — deferred. Plan doc at `docs/remote-control-m3-elevated-switching.md`. Bisect rc.7 → rc.21 on PC50045 to identify the breaking commit.
- **Mailpit / Cycle 4** infra (from `memory/project_next_session_cycle4_handover.md`) — separate effort, untouched.
- **Task #166 (rc.19 P8 manual smoke)** — pending. rc.23 verification (above) subsumes it.

## Files touched this session

- `agents/roomler-agent/src/logs_fetch.rs` — NEW (180 LOC + 5 tests)
- `agents/roomler-agent/src/lib.rs` — wire `pub mod logs_fetch`
- `agents/roomler-agent/src/peer.rs` — `rc:logs-fetch` arm + `dc_for_reply` clone
- `agents/roomler-agent/src/files.rs` — `#[cfg_attr(not(target_os = "windows"), allow(dead_code))]` on staging-strategy symbols
- `agents/roomler-agent/src/config.rs` — `CURRENT_SCHEMA_VERSION` bump
- `agents/roomler-agent-tray/tauri.conf.json` — version bump
- `Cargo.toml` — workspace.version → `0.3.0-rc.23`
- `Cargo.lock` — propagated bump
- `ui/src/composables/useRemoteControl.ts` — infinite reconnect + `rc:logs-fetch.reply` parsing + `fetchAgentLogs` + `agentLogs`/`agentLogsLoading` refs
- `ui/src/__tests__/composables/useRemoteControl.spec.ts` — `nextReconnectDelayMs` spec rewritten
- `ui/src/views/remote/RemoteControl.vue` — log viewer dialog + Staging quick-access button + auto-scroll/refresh wiring
- `ui/e2e/remote-upload-pc50045.spec.ts` — NEW (Playwright E2E against PROD)
- `.gitignore` — added `.cred`
- `HANDOVER20.md` — this file

## Commits this session

- `f326737` — feat(remote): rc.23 P1 — infinite reconnect + E2E spec for PC50045 upload
- `67e23a7` — feat(remote): rc.23 P2 — agent log viewer + staging quick-access
- `c89ab79` — fix(ci): silence dead_code warning on non-Windows for STAGE_IN_PROGRAMDATA

## How to pick up next session

1. Verify mars docker build completed + rc.23 image is on the registry.
2. Verify ArgoCD reconciled the new image to k8s.
3. `curl -sI https://roomler.ai/health` should show rc.23.
4. Wait for `agent-v0.3.0-rc.23` release workflow to publish MSIs.
5. Walk the user through the 7-step verification flow above.
6. Iterate based on what the log viewer surfaces during the failed upload.
