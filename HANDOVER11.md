# Handover #11 — 0.1.60 → 0.2.5: M5 verification, M3 Z-path complete, A1 NO-GO confirmed

> Continuation of HANDOVER10 (which closed at 0.1.41 with VP9 4:4:4
> shipped + ArgoCD GitOps live). This window covered the full M5
> verification + M3 Z-path implementation + multiple CI fixes for
> the per-Machine MSI flavour, plus an empirical NO-GO finding for
> the A1 (real-Winlogon-control) architecture via WGC.
>
> **Eleven releases shipped: 0.1.61, 0.1.62, 0.1.63, 0.2.0, 0.2.5.**
> (0.2.1 / 0.2.2 / 0.2.3 / 0.2.4 tags exist on the repo but their
> CI builds failed; 0.2.5 ships everything they were meant to.)

## Releases timeline

| Tag | Substance | CI |
|---|---|---|
| 0.1.61 | M5 verification harness (`scripts/m5-verify-win11.ps1`) + clean-exit-not-a-crash fix in 3 places (user crash counter, supervisor on update-exit, supervisor restart race) | green |
| 0.1.62 | Auto-update install-storm prevention via 5-min cooldown marker | green |
| 0.1.63 | Cooldown log visibility fix (line moved before 24h sleep) | green |
| (mid) | M3 phase 1: `SpawnDecision::SystemContextCapture` variant + `keep_stream_alive` arg | n/a (no tag) |
| (mid) | WGC session-0 spike binary `system-capture-smoke` + `desktop.rs` Win32 wrappers | n/a |
| (mid) | Browser auto-reconnect ladder 250ms→8s, 6-attempt cap | n/a |
| (mid) | Mobile-friendly RemoteControl toolbar (selects collapse to bottom-sheet on `<md`) | n/a |
| (mid) | Lock-state monitor (`lock_state.rs`) + lock-overlay producer (`lock_overlay.rs`) | n/a |
| 0.2.0 | M3 phase 3c: lock-overlay wired into capture pumps + 0.2.0 minor cut | green |
| 0.2.1 | per-Machine MSI flavour | **failed CI** |
| 0.2.2 | input drop-on-lock | **failed CI** |
| 0.2.3 | `rc:host_locked` control-DC signal + viewer toolbar badge | **failed CI** |
| 0.2.4 | CI fix: relocate perMachine WXS out of cargo-wix scan path; escape `--` in comments | **failed CI** (separate ICE38/43/57 issue) |
| 0.2.5 | drop ApplicationShortcut from perMachine MSI to satisfy ICE38/43/57 | **green** — ships everything 0.2.1-0.2.4 substance + the CI fix |

## What ships in 0.2.5 operationally

When the user-context worker observes the input desktop transition to `winsta0\Winlogon` (Win+L lock, UAC, sign-out):

1. Within ~500 ms: the captured frame is replaced with a synthesised "Host is locked" overlay frame (centred yellow padlock badge on dark grey, ~10 KB H.264 keyframe). Browser keeps streaming.
2. A yellow `mdi-lock` "Host locked" chip appears in the viewer toolbar (via the `rc:host_locked` control-DC signal).
3. Input events from the browser are dropped early at the agent — no enigo log spam, no events into the wrong desktop.
4. Browser's WebRTC peer stays connected throughout. If a transient drop happens, the auto-reconnect ladder (250ms → 8s, 6-attempt cap) restores the session without the operator hitting F5.
5. When the user unlocks the host, both transitions force an encoder keyframe so the real desktop snaps back into view immediately.

For fleet/IT deployment: `roomler-agent-<v>-perMachine-x86_64-pc-windows-msvc.msi` (new in 0.2.5) installs under `%ProgramFiles%\roomler-agent`, registers the SCM service automatically via a deferred non-impersonated custom action. UAC fires once. Mutually-exclusive launch conditions prevent dual-install with the perUser MSI.

## A1 NO-GO finding (the M3 cycle's hard-blocked extension)

Z-path ships the "host is locked" experience cleanly, but does **not** support remote-unlock (operator typing the password into the lock screen via the viewer). That capability requires SYSTEM-context capture+input on `winsta0\Winlogon` — the A1 architecture.

A1 needed empirical confirmation that WGC works against Winlogon from SYSTEM. Smoke binary `system-capture-smoke` exists for this purpose (commit `9619c83`). Run via:

```powershell
@"
"$env:LOCALAPPDATA\Programs\roomler-agent\roomler-agent.exe" system-capture-smoke --desktop winlogon --frames 3 --timeout-ms 5000 > C:\Windows\Temp\smoke.txt 2>&1
"@ | Out-File -Encoding ascii C:\Windows\Temp\smoke.bat
psexec -s -i 1 -accepteula C:\Windows\Temp\smoke.bat
Get-Content C:\Windows\Temp\smoke.txt
```

**Result on PC50045 2026-05-02:**
```
system-capture-smoke: target=Winlogon frames=3 timeout_ms=5000
  before-attach desktop = "Default"
  after-attach  desktop = "Winlogon"
  D3D11 device created
  IDirect3DDevice wrapped
  HMONITOR resolved
Error: IGraphicsCaptureItemInterop::CreateForMonitor

Caused by:
    Der angegebene Dienst ist kein installierter Dienst. (0x80070424)
```

`0x80070424` = `HRESULT_FROM_WIN32(ERROR_SERVICE_DOES_NOT_EXIST)`. WGC's WinRT activation chain can't reach a service it depends on from session 0. Consistent with broader public knowledge — Microsoft has never officially supported WGC from SYSTEM context.

## Next session brief (memory `project_m3_a1_investigation.md`)

**Investigate BEFORE coding A1.** Three queued probes, in priority order:

1. **Reproduce RustDesk's approach**. RustDesk DOES support remote control of the Windows lock screen. Their canonical reference solves the problem we hit. Read `github.com/rustdesk/rustdesk` (specifically `libs/scrap/` and `src/platform/windows.rs`) + their issue tracker for `winlogon` / `secure desktop` discussions. Specifically: which capture API do they use under SYSTEM? How do they handle the desktop-switch problem? Is there a service-startup workaround? Do they use a separate SYSTEM helper process or driver?
2. **Probe the missing-service path**. 0x80070424 names a missing service. Use Process Monitor (Sysinternals) during a smoke run to capture `OpenServiceW` calls returning `ERROR_SERVICE_DOES_NOT_EXIST`. Try starting candidate WinRT services (`AppXSvc`, `tabletinputservice`, `RuntimeBroker`-related, `ClipSVC`) and re-run the smoke. If one flips the result → operator runbook is the workaround.
3. **DXGI Desktop Duplication fallback** (only if 1 + 2 yield nothing). Add a `--backend dxgi` flag to the smoke binary. Known gotchas: needs SYSTEM session GPU bind, no HW cursor overlay.

**Decision criteria.** After investigation:
- RustDesk's approach replicable → commit to that architecture
- RustDesk uses non-portable (signed kernel driver) → close A1, ship Z-path as final
- Missing-service probe finds a single fixable service → ship A1 with operator runbook step

## Files of interest for the next session

- `agents/roomler-agent/src/win_service/capture_smoke.rs` — the spike binary; add new backends here as new `--backend` flag values
- `agents/roomler-agent/src/win_service/desktop.rs` — Win32 desktop FFI wrappers (already proven working from SYSTEM)
- `agents/roomler-agent/src/win_service/supervisor.rs` — has the `SpawnDecision::SystemContextCapture` variant ready to wire when A1 lands
- `agents/roomler-agent/src/lock_state.rs` — desktop-transition detection (production)
- `agents/roomler-agent/src/lock_overlay.rs` — Z-path overlay frame producer (production)
- `agents/roomler-agent/src/peer.rs::media_pump` — encoder pump; Z-path swap is at the `apply_target_resolution` call site

## Sharp edges discovered + closed

- **Auto-update install-storm** under SCM supervision (e069019l, 2026-05-02): worker spawn → detect new release → spawn installer → exit code=0 → supervisor respawns → next worker sees same pending update → fires another installer. Tightened to ~1.5 s/cycle by the 0.1.61 `code=0 → no backoff` patch. Fix: 5-min cooldown marker (0.1.62), log visibility (0.1.63).
- **WiX `--` rule violations** in comments. Bit twice this cycle (`cfc2b5a` originally, then again in 0.2.4's perMachine WXS prose). Workaround: paraphrase to `[hyphen][hyphen]flag-name`.
- **cargo-wix auto-discovers all `*.wxs` in `wix/`** — placing `main-perMachine.wxs` next to `main.wxs` made the per-User build also try to compile the perMachine source. Fix: relocate to sibling `agents/roomler-agent/wix-perMachine/main.wxs` (outside cargo-wix's scan path), CI swaps via copy.
- **WiX ICE38/43/57** for ProgramMenuFolder + Shortcut under perMachine. Even HKLM KeyPath doesn't satisfy the validator. Fix: drop the Start Menu shortcut from the perMachine MSI; per-User MSI keeps its shortcut.
- **API server release-proxy cache** at `roomler.ai/api/agent/latest-release`. Caches GitHub Releases response; can serve stale data for some interval. Workaround: `ROOMLER_AGENT_UPDATE_URL=https://api.github.com/repos/gjovanov/roomler-ai/releases/latest roomler-agent self-update` to bypass.
- **psexec output capture**: psexec with `-i` attaches the spawned process to the target session's console; stdout doesn't relay back. Workaround: write a `.bat` file that redirects to a temp file, run that via psexec, read the file after.
- **PowerShell `$env:LOCALAPPDATA` from elevated vs non-elevated**: same value when UAC-elevated within the same user account; different when running as a different admin account. Pre-cooldown verification on 2026-05-02 was bitten by reading from the elevated shell's profile path while the agent worker (running as `e069019l`) wrote to a different profile.

## State at end of session

- **master HEAD**: `d535899` (email update follow-up to 0.2.5 tag; will ride next release)
- **Latest GitHub Release**: `agent-v0.2.5` (2026-05-02 22:39:56Z)
- **Tests**: 177 agent-lib + 370 frontend (was 162 + 356 at session start)
- **No open in-flight commits** beyond the email update
- **A1 architecture**: empirically blocked on WGC; investigation queue ready for next session

## Useful command snippets for next session

**Bypass the release-proxy cache** to force-update an agent:
```powershell
$env:ROOMLER_AGENT_UPDATE_URL = "https://api.github.com/repos/gjovanov/roomler-ai/releases/latest"
& "$env:LOCALAPPDATA\Programs\roomler-agent\roomler-agent.exe" self-update
```

**Re-run the WGC smoke** (after starting a candidate service):
```powershell
psexec -s -i 1 -accepteula C:\Windows\Temp\smoke.bat
Get-Content C:\Windows\Temp\smoke.txt
```

**Process Monitor filter for the smoke run** (from elevated PS, requires Sysinternals):
```
Procmon → Filter:
  Process Name contains "roomler-agent.exe"
  Operation is "RegOpenKey" / "RegQueryValue" / "Process Create" / "Load Image"
  Result is not "SUCCESS"
```
Run smoke; stop Procmon; export to CSV; grep for `OpenServiceW` or service-related errors near the `0x80070424` time window.
