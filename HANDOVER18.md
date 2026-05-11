# HANDOVER18 — rc.21 status + 2 open field bugs for fresh session

> Continuation of HANDOVER17. rc.20 + rc.21 shipped end-to-end this
> session. Two field bugs surfaced on PC50045 that need a clean
> session to investigate properly.

## State at session end

- **Master HEAD**: rc.21 fix committed (`b2f6437`) + version bumped (`715551f` → ... → tag `agent-v0.3.0-rc.21` published)
- **Prod web**: `v20260511-976cf8fba0ad` → `roomler.ai/health` reports `0.3.0-rc.21`
- **Agent release**: `agent-v0.3.0-rc.21` published with all 11 assets
- **PC50045 on**: `roomler-agent 0.3.0-rc.21` (manually installed via perMachine MSI)
- **Active user on PC50045**: `e069019` (NOT `extjovanov` as initially assumed — that's a different account on the same box)

## What rc.21 fixed

`agents/roomler-agent/src/files.rs::download_dir()` — Windows-only final fallback changed from `std::env::temp_dir()` (= `C:\Windows\SystemTemp\`, aggressively scanned by Defender) to `%PROGRAMDATA%\roomler\roomler-agent\uploads\` (SYSTEM-writable, persistent, not auto-scanned). Closed the issue where uploads under Folder-Redirected user profiles ended up in SystemTemp and got blocked by Defender's `.exe`-scan policy.

For e069019 on PC50045 specifically: `active_user_downloads_path()` resolves to `C:\Users\e069019\Downloads` (his Downloads is NOT redirected — only `extjovanov`'s was), so the PROGRAMDATA fallback isn't reached. Small uploads land successfully at `C:\Users\e069019\Downloads\<file>` as expected.

## Open Bug 1: Large file uploads still fail on PC50045

**Symptom**: Files >~1 MB fail with "Upload failed: reconnect budget exhausted after 6 attempts". Small files (text, .sql, tiny .zip) succeed and land at `C:\Users\e069019\Downloads\`.

**Confirmed working**: 3 KB .sql, 11 KB .zip.
**Confirmed failing**: 14 MB .exe.

**Working hypothesis** (REVISED 2026-05-12): **ESET Security real-time scanner** intercepts the staging `data` file writes. PC50045 has `C:\Program Files\ESET\ESET Security\ekrn.exe` running — the user's box is an ESET-protected corporate Win11, NOT Defender. Initial Defender-exclusion test was a misfire (`Add-MpPreference` adds Defender exclusion; doesn't affect ESET). User-side test confirmed: large .zip still fails after Defender exclusion was added.

PC50045 ESET stack:
- `ekrn.exe` — ESET kernel service
- `efwd.exe` — ESET firewall daemon
- `egui.exe`/`eguiProxy.exe` — ESET GUI
- `ERAAgent.exe` — ESET Remote Administrator (managed by central console)

The box is also heavily managed: SCCM (`C:\Windows\ccm`, `ccmsetup`), Tanium endpoint client (`C:\Program Files (x86)\Tanium\Tanium Client`), Dameware remote support. Multiple users on the same machine (`e069019`, `extjovanov`, `f002459`, `sichern2`, `Werkstatt`). Adding ESET exclusions probably requires the central ESET Remote Admin console; can't be done locally.

**Three options for rc.22**:

**Option A**: **Compress or obfuscate the staging content** so ESET's pattern matchers don't recognize it as a PE binary mid-write. E.g., write the file in chunks XOR'd with a key, then decode at end-time rename. Complex; might trigger different AV heuristics; fragile.

**Option B**: **Stage in a heavily-protected SYSTEM location** ESET likely whitelists. Candidates:
- `%PROGRAMDATA%\roomler-agent\staging\<id>\` — same place as `peer-connected.lock`; if `ERAAgent.exe` whitelists `ekrn`'s own folder pattern, our PROGRAMDATA might inherit similar leniency. UNTESTED. Cross-volume rename to user Downloads at end-time costs ~200 ms on SSD for 14 MB.
- `C:\Windows\Temp\<id>\` — system Temp; sometimes excluded for ConfigMgr operations. UNTESTED.
- The agent's own install dir `C:\Program Files\roomler-agent\staging\` — but installer-folder writes from a service typically trip AV more, not less.

**Option C**: **Throttle chunk writes** to a rate that lets ESET scan each chunk in real time without backing up SCTP. E.g., 1 chunk per 50ms = 1.3 MiB/s sustained. Reasonable for control-plane operations; slow for big uploads. Browser side: clamp pump rate. Tested fix shape only — doesn't guarantee resolution.

**Option D** (the right long-term fix): **Document that operators need an ESET / corporate-AV exclusion for `C:\Program Files\roomler-agent\` + the staging path**, and provide a one-line `eset_admin` request for ops teams to file. Add the request template into the rc.22 release notes.

**For the next session**:
1. Have user try uploading a 5 MB file (between 11 KB success threshold and 14 MB failure) to identify the exact SIZE threshold. Tells us if it's chunk-count-based or absolute-size-based.
2. Tail agent log during a failed large upload — look for chunk-callback warnings, write_all error returns, anything ESET-related.
3. Check ESET log if accessible (typically `C:\ProgramData\ESET\ESET Security\Logs\` — may require admin).
4. Test Option B with a quick patch: hard-code staging at `%PROGRAMDATA%\roomler\roomler-agent\staging\` ALWAYS (not just on Downloads-redirect fallback). One-line change.
5. If Option B fails, escalate to Option D — request ESET exclusion via corporate IT.

**Files of interest**:
- `agents/roomler-agent/src/files.rs::chunk()` — the per-chunk write loop, ~line 460
- `agents/roomler-agent/src/peer.rs::handle_files_control` — the on_message dispatcher that calls chunk()
- `ui/src/composables/useRemoteControl.ts::innerPump` — the browser-side pump (the wrapper that gives "budget exhausted")

## Open Bug 2: Mouse control fails over admin-pwsh windows

**Symptom**: When the operator hovers an elevated/admin pwsh window remotely, mouse stops responding. Click + keyboard may also stop (user wasn't precise on this).

**Expected behavior** (per memory `project_m3_a1_implementation.md`): rc.7 verified that "input on admin apps + clipboard work" because SystemContext worker runs as LocalSystem (S-1-5-18), which bypasses UIPI (User Interface Privilege Isolation) — SYSTEM can inject input into any window regardless of integrity level.

**Confirmation that SystemContext IS active** (from agent log 2026-05-11):
```
INFO system-context capture: backend=DXGI
INFO capture: backend=system-context (DXGI + GDI fallback for SYSTEM-context worker)
INFO system-context input: thread already bound to input desktop at startup
INFO input: backend=system-context (enigo with SetThreadDesktop rebind)
```

So SystemContext is on. But admin input doesn't work. Something regressed between rc.7 (2026-04-26) and rc.21 (today).

**Bisect plan** for the next session:
1. Pull the agent MSI for rc.10, rc.13, rc.16, rc.18, rc.20 from GitHub Releases.
2. Install each in sequence on PC50045 (with rc.20 MSI cleanup-legacy-install handling cross-flavour transitions cleanly).
3. After each install, attempt admin-pwsh input + log result.
4. Bisect to find the breaking RC.
5. Inspect that RC's diff against the prior RC for input-thread / desktop / lock-state changes.

**Candidate regression points**:
- **0.2.7 access-mask reduction** (`project_input_regression_0_2_x.md`): "access mask reduced to GENERIC_READ". The reduction was for token-dup; might've reduced what the input thread can do.
- **rc.20 lock_state.rs poll** (M3 Z-path): polls `OpenInputDesktop` every 500ms; on transition to non-Default it sets `LockState::Locked` and the input handler drops events early (`attach_input_handler` consumes `lock_state` receiver). If UAC's Secure Attention Sequence (Ctrl+Alt+Del moment) is mis-flagged as "locked", input stops. But admin pwsh shouldn't trigger SAS — it should stay on `winsta0\Default`. Could be a wider misdetection.
- **rc.16-rc.18 SetThreadDesktop rebind logic** changes — input thread retains binding from startup; doesn't re-acquire when input desktop switches.

**Files of interest**:
- `agents/roomler-agent/src/lock_state.rs` — the LockState watch + 500ms poll
- `agents/roomler-agent/src/input/` — the input thread (enigo rebind logic)
- `agents/roomler-agent/src/system_context/system_context_probe.rs` — system context probe (worker_role detection)
- `agents/roomler-agent/src/peer.rs::attach_input_handler` — consumes the lock_state receiver and gates input events

## Recommended next-session plan

1. **5 min** — fire the Defender-exclusion test for upload bug. If confirmed, fold into rc.22.
2. **30 min** — bisect rc.7 → rc.21 for admin-pwsh-input regression on PC50045.
3. **1-2 hours** — implement fix for whichever RC introduced the regression. Likely small (a misplaced `if locked` guard, a missing desktop rebind, etc.).
4. **30 min** — manual smoke on PC50045 + tag rc.22.
5. **15 min** — bundle the Defender exclusion into the WiX install custom action if test (1) confirms it.

## Files touched this session (rc.20 + rc.21 cycle)

- `agents/roomler-agent/src/files.rs` — major: rc.19 staging/sweep + rc.21 PROGRAMDATA fallback
- `agents/roomler-agent/src/peer.rs` — rc.19 Resume + Cancel arm
- `agents/roomler-agent/src/updater.rs` — rc.19 ACTIVE_TRANSFERS gate
- `agents/roomler-agent/src/encode/caps.rs` — rc.19 "resume" cap
- `agents/roomler-agent/src/config.rs` — schema version bumps
- `agents/roomler-agent/src/main.rs` — rc.19 sweep_orphans pre-WS-connect
- `agents/roomler-agent/tests/file_dc.rs` — rc.19 resume wire-format test
- `agents/roomler-agent-tray/Cargo.toml` + `tauri.conf.json` — rc.20 custom-protocol fix + version bumps
- `ui/src/composables/useRemoteControl.ts` — rc.19 P4-P5 browser auto-resume pump
- `ui/src/views/remote/RemoteControl.vue` — rc.19 P7 UI polish
- `Cargo.toml` — version bumps
- `CLAUDE.md` + `HANDOVER16.md` + `HANDOVER17.md` — docs

## How to pick up next session

1. Read this file first.
2. Have user run the Defender exclusion test on PC50045 (5 min).
3. While waiting for that result, start the bisect — pull rc.10 MSI from GitHub Releases as the starting point (rc.7 itself didn't have a perMachine MSI flavour).
4. The user's PC50045 admin pwsh setup is the canonical reproduction. Fast iteration possible since they're actively testing.

Open the agent log file at `C:\Windows\System32\config\systemprofile\AppData\Local\roomler\roomler-agent\data\logs\roomler-agent.log.<date>` early — it'll likely be the highest-signal evidence source.
