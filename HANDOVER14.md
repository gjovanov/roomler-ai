# HANDOVER14 — M3 A1 cycle 0.3.0-rc.1 → rc.7 field-validated

## State at session end

- **Master HEAD**: `8cff0db` (rc.7 supervisor "always SystemContext when env var on" fix)
- **Latest tag**: `agent-v0.3.0-rc.7` (CI building at session end)
- **Field host**: PC50045 (e069019), Win11 24H2, Intel Iris Xe, perMachine MSI installed
- **Lib tests**: 293 with `--features full-hw,system-context`, 211 default
- **Default + perUser MSI behaviour**: byte-for-byte unchanged from 0.2.7
- **Auto-swap**: env-var-gated, default-off; with `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP=1` SystemContext is the only worker

## What got fixed this session (rc.1 → rc.7)

| Tag | What |
|---|---|
| rc.1 | Initial M3 A1 cut: capture_pump + system_context_backend + supervisor swap arms + peer-presence marker |
| rc.2 | Peer-presence diagnostics (worker first-write log, supervisor transition log, `peer-presence-status` CLI) |
| rc.3 | Marker writes timestamp body (NTFS mtime no-op-on-empty bug) |
| rc.4 | Auto-swap default-OFF behind env var (was crash-looping in field) |
| rc.5 | `parse_version` ranks rc.N pre-releases (rc.3 == rc.4 == (0,3,0) bug) |
| rc.6 | SystemContext worker reads config from active user's profile (rc.5 had SYSTEM-profile config-load failure → exit code=1) |
| rc.7 | Drop swap entirely — SystemContext from cold start when env var on (swap window > browser auto-reconnect ladder) |

## Field-validated on PC50045 with rc.7 + env var ON

User's actual test results (this session):

| Feature | Status |
|---|---|
| Browser connection establishes | ✅ Works (SystemContext from cold start, no swap window) |
| Mouse/keyboard input on **non-admin** apps | ✅ Works |
| Mouse/keyboard input on **admin PowerShell** | ✅ **Works** — UIPI bypassed via SYSTEM integrity |
| Clipboard sync (copy host → paste browser, vice versa) | ✅ Works both directions |
| File upload — **browse + select file** | ❌ **Hangs** — spinner icon, never completes |
| File upload — **drag & drop** | ❌ **Broken UX** — browser opens the dragged image in a new tab instead of intercepting |
| Cursor shape streaming — arrow / pointer | ✅ Visible in browser |
| Cursor shape streaming — **I-beam (text cursor)** | ❌ **Not visible** when hovered over Notepad++ text area |
| Lock-screen control (Win+L → unlock from browser) | ⏸️ Not yet field-tested in this session |
| `m3-a1-verify-win11.ps1 -Action Latency` | ⏸️ Not yet run with rc.7 |

## What's queued for the next session

### P0 — bugs from field testing rc.7

1. **File upload hangs** when triggered via browse-and-select file picker.
   - Likely cause: file-DC handler resolves `%USERPROFILE%\Downloads` which under LocalSystem becomes `C:\Windows\System32\config\systemprofile\Downloads\`. The directory might not exist; write fails silently OR succeeds but the user can never find the file.
   - Fix: same shape as the rc.6 config-path fix — when in SystemContext mode, resolve the active user's `%USERPROFILE%` via `WTSQuerySessionInformationW(WTSUserName)` and write to the user's actual Downloads folder.
   - Investigation: locate the file-DC handler in `peer.rs` (look for `files:begin` / `files:write` / `files:end` JSON handling), see how it computes the destination path, add SystemContext fallback.

2. **Drag & drop file upload broken UX** — browser opens dropped image in new tab.
   - Cause: the viewer's drop handler isn't calling `event.preventDefault()` early enough OR doesn't suppress the default browser file-open behaviour.
   - Fix: in `ui/src/views/remote/RemoteControl.vue`, ensure drag-over and drop event listeners call `preventDefault()` and `stopPropagation()` on every `dragenter` / `dragover` / `drop` to override browser defaults.
   - Browser-side only — no agent change needed.

3. **Cursor I-beam shape not visible** in browser when hovering over text fields.
   - Cause: cursor shape detection in `capture/cursor.rs` likely doesn't include the I-beam OCR_IBEAM cursor handle, OR the streaming code falls back to "no cursor visible" when the OS cursor is one of the system non-arrow shapes.
   - Fix: add I-beam (`IDC_IBEAM`) to the cursor shape mapping. Test other non-arrow cursors too (hand, wait, crosshair).
   - Investigation: `capture/cursor.rs` — see what shapes are currently translated.

### P1 — verification harness gaps

4. **`m3-a1-verify-win11.ps1 -Action Latency`** says "No supervisor log found" — script is looking in the wrong path.
   - Fix: update the script to search `C:\Windows\System32\config\systemprofile\AppData\Local\roomler\roomler-agent\data\logs\roomler-agent.log.YYYY-MM-DD` (LocalSystem profile), not whatever it's looking in now.

5. **Lock-screen / UAC end-to-end test** not yet run with rc.7 in field. Once P0 #1–3 are fixed, run:
   - `m3-a1-verify-win11.ps1 -Action LockUnlockCycle`
   - `m3-a1-verify-win11.ps1 -Action Latency`
   - Required gates per plan: p50 < 1.5s, p99 < 3.0s for desktop transitions.

### P2 — promote 0.3.0 final

6. After P0 + P1 pass, drop the rc suffix. Cut `agent-v0.3.0`. Update memory + this handover. Mark task #119 complete.

### Deferred — post-0.3.0 architectural work

7. **Session-continuity-preserving swap design** (currently the env-var-on path always uses SystemContext, losing user-context features even when not needed). The proper fix is to have the supervisor own the WS connection so it can swap workers without tearing down the browser session. Significant scope; out for 0.3.0.

8. **Pre-flight #4** (Task #115): `WTSEnumerateSessions` vs `WTSGetActiveConsoleSessionId` on RDP-only host. Still pending. Not blocking 0.3.0 since the field box is non-RDP.

## Useful artefacts

- `agents/roomler-agent/scripts/m3-a1-verify-win11.ps1` — operator harness
- `agents/roomler-agent/src/system_context/peer_presence.rs::snapshot()` — diagnostic API used by the CLI command
- `agents/roomler-agent/src/system_context/user_profile.rs::active_user_config_path()` — pattern to copy for the file-DC P0 fix (resolves user APPDATA from SystemContext)
- Supervisor log path on perMachine: `C:\Windows\System32\config\systemprofile\AppData\Local\roomler\roomler-agent\data\logs\roomler-agent.log.YYYY-MM-DD`
- Worker log path (user-context spawn): `C:\Users\<user>\AppData\Local\roomler\roomler-agent\data\logs\roomler-agent.log.YYYY-MM-DD`
- Marker file: `C:\ProgramData\roomler-agent\peer-connected.lock` (mtime is the heartbeat; visible via `peer-presence-status` CLI)

## Resume checklist for next session

1. Read this HANDOVER14.md
2. Check rc.7 CI completed and assets are up at https://github.com/gjovanov/roomler-ai/releases/tag/agent-v0.3.0-rc.7
3. Begin P0 #1 (file-DC user-profile fallback) — pattern is in `system_context/user_profile.rs`
4. P0 #2 (drag/drop preventDefault) — quick UI fix
5. P0 #3 (cursor I-beam shape) — investigate `capture/cursor.rs`
6. Then P1, P2, and call M3 A1 done.

## Sharp edges discovered during this cycle

- **NTFS LastWriteTime is a no-op when content doesn't change.** `fs::write(path, b"")` on an existing 0-byte file does NOT advance mtime. Always write varying content for mtime-based heartbeats.
- **`parse_version` semver-stripping is dangerous.** Stripping `-rc.N` from the patch component conflates rc.3/rc.4/rc.5 into the same tuple; auto-updater can't differentiate. Now uses 4-tuple `(major, minor, patch, pre_rank)` where `pre_rank=u64::MAX` for final and `N` for `-rc.N`.
- **SCM service spawn-as-SYSTEM-in-session-N has profile-path mismatches.** Anything that resolves via `%USERPROFILE%`, `%APPDATA%`, `%LOCALAPPDATA%` will hit the LocalSystem profile (`C:\Windows\System32\config\systemprofile\…`) — NOT the user's profile. Each affected feature needs an active-user-profile fallback.
- **Browser auto-reconnect window (~16s) is shorter than supervisor swap window (~13s)** for cold-start work (config + preflight + caps probe + agent.hello). Architecturally, mid-session swaps are not viable without server-side WS holding.
- **UIPI blocks user-context input from reaching admin apps.** Only SYSTEM-integrity (or higher) processes can `SendInput` to elevated/lock-screen targets. M3 A1 SystemContext path is the only way to get this on perMachine deployments without code-signing + UIAccess manifest gymnastics.
- **`Application SID does not match Conductor SID`** — Win11 RestartManager noise during MSI upgrades. Cosmetic; the upgrade succeeds. Ignorable.
