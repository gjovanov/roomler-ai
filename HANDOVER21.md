# HANDOVER21 — end-of-rc.26 cycle + next-session install wizard plan

> The rc.22 → rc.26 cycle closed both HANDOVER18 bugs + delivered M3 A1
> (lock-screen drive from web). This handover lists what's left + lays
> out the next session's focus: a polished installation experience so
> operators don't have to copy-paste PowerShell to deploy roomler-agent.

## Cycle ship status

| RC | What shipped | Status |
|---|---|---|
| 0.3.0-rc.22 | always-PROGRAMDATA staging on Windows (speculative ESET fix) | ✓ live |
| 0.3.0-rc.23 | infinite reconnect / log viewer / staging shortcut + 5 web hotfixes | ✓ live |
| 0.3.0-rc.23 hotfix #3 | **64 KiB → 16 KiB SCTP chunks — closed HANDOVER18 Bug 1** | ✓ live |
| 0.3.0-rc.24 | rc:logs-fetch reply streaming + post-disconnect indicator teardown + 256 KiB FSYNC + staging path fix | ✓ live |
| 0.3.0-rc.25 | M3 Change A (probe input desktop directly) + diagnostic logs | ✓ live |
| 0.3.0-rc.26 | **M3 A1 — lock-screen drive enabled under SystemContext** | ✓ live |

**Tagged + released on GitHub**: `agent-v0.3.0-rc.22` … `agent-v0.3.0-rc.26`. All MSI / tray / .deb / .pkg artifacts published.

**Prod web** at `roomler.ai/health` → currently `0.3.0-rc.24` (`v20260513-219ff5554ec7` or later post-rc.25 hotfixes). Web has all rc.23+ improvements live.

**PC50045 field state**: rc.26 agent installed (perMachine MSI) + `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP=1` set on the SCM service. Confirmed working:
- Large file uploads (15 MB .exe + 1 MB CV.pdf both round-trip)
- Agent log viewer (chunked replies for 1000+ lines)
- Staging quick-access button (navigates to `C:\ProgramData\roomler\roomler-agent\staging`)
- Lock screen → browser shows real Winlogon UI → operator types password → host unlocks

## Outstanding from the rc.22–rc.26 cycle

### Status uncertain (probably fine, never explicitly verified)

1. **HANDOVER18 Bug 2 — admin pwsh input regression** — never explicitly confirmed fixed. The rc.25 diagnostic logs showed no false `Locked` transitions and no `dispatch error` lines during normal use, so the bug may have been transient or already addressed by Change A. **Next-session task**: have the user test "hover admin pwsh + click + type" specifically, capture log; if clean → close Bug 2 officially.
2. **Post-deploy stuck overlay** — rc.24 added `close_all_peers(.., &indicator)` teardown on every WS disconnect path. Field has not yet validated this across an actual web deploy (need a roomler.ai web roll while the user is connected from a browser, then observe whether the red frame disappears on PC50045 within seconds). **Next-session task**: confirm.
3. **FSYNC_THRESHOLD lowered to 256 KiB** in rc.24 — never benchmarked end-to-end on Windows with Defender / ESET active. Theoretical overhead is acceptable; in-the-field measurement is nice-to-have but not blocking.

### Genuinely open

1. **E2E test against PC50045 via PROD** (`ui/e2e/remote-upload-pc50045.spec.ts`) — written in rc.23 but never executed against the actual production deployment. Task #176 is still pending. Could be a one-line wakeup once we want CI coverage of the full upload path.
2. **Agent release CI runs unsigned** — both Windows MSI and macOS .pkg ship "unsigned" labels in their filenames. Signing certs need to be provisioned + wired into the workflow. Tracked in the release-agent.yml comments. **Not blocking** end-user installs because the SCM service deployment doesn't trigger Gatekeeper / SmartScreen pre-1-year-old-trust on the same machine, but operators on fresh hosts get a warning prompt.
3. **Memory updates**: my auto-memory needs entries for "SCTP 16 KiB chunk lesson" and "A1 lock-screen drive enabled in rc.26 under SystemContext". Both are field-validated lessons that prevent future regressions.

## Next session — installation + onboarding wizard

The user's directive: "make installation/onboarding smooth — wizard for user vs service mode, token registration with display of existing, uninstall old versions, everything that makes it polished."

Today's flow is painful:
1. Download the right MSI (perUser or perMachine — operators get confused)
2. Run it (silent install with no progress)
3. For service mode: run elevated PowerShell with `service install --as-service`
4. For SystemContext: run elevated `reg add` to set `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP=1` + restart service
5. Enroll with a token via CLI `roomler-agent enroll --server ... --token ...`
6. Pray it connects

We have an existing Tauri tray app (`agents/roomler-agent-tray/`) that does basic post-install onboarding (paste token, device name). We need to expand it into a full installer-wizard that handles steps 1-6 above visibly.

### Wizard design

**Recommended architecture**: ship the wizard as part of the Tauri tray app's first-run flow. The tray app is already in the release pipeline (rc.18+); the MSI installer can register the tray app to launch automatically on first run after install. Alternative: build a separate `roomler-installer.exe` Tauri app that runs BEFORE the MSI — operator downloads one .exe instead of choosing between MSIs.

**Recommended path**: separate `roomler-installer.exe`. Reason: the tray-app approach requires the operator to know which MSI to download first, which is what we're trying to eliminate.

### Wizard step-by-step (target experience)

```
┌─────────────────────────────────────────────────────────────┐
│ Step 1 of 5 — Welcome                                       │
├─────────────────────────────────────────────────────────────┤
│ Roomler Agent installer.                                    │
│                                                             │
│ [✓] Detected: this PC is running roomler-agent 0.3.0-rc.25  │
│     (perMachine SCM service, SystemContext mode)            │
│                                                             │
│ This installer will:                                        │
│  • Upgrade to 0.3.0-rc.26                                   │
│  • Preserve your existing token + machine_id                │
│  • Take ~30 seconds                                         │
│                                                             │
│   [ Cancel ]                                  [ Continue ]  │
└─────────────────────────────────────────────────────────────┘
```

OR if no install detected:

```
┌─────────────────────────────────────────────────────────────┐
│ Step 1 of 5 — Welcome                                       │
├─────────────────────────────────────────────────────────────┤
│ Roomler Agent installer.                                    │
│                                                             │
│ [⚪] No existing install detected.                          │
│                                                             │
│ Choose a deployment mode:                                   │
│                                                             │
│  ( ) User mode (Z-path)                                     │
│      • Runs as you, on logon                                │
│      • You can remote-view but NOT remote-unlock the lock   │
│        screen or drive elevated apps                        │
│      • No admin needed to install                           │
│                                                             │
│  (●) Service mode (A1-path) — recommended for IT support    │
│      • Runs as LocalSystem from boot                        │
│      • You can remote-unlock the host and drive elevated    │
│        apps + UAC prompts                                   │
│      • Needs admin once at install                          │
│                                                             │
│   [ Cancel ]                                  [ Continue ]  │
└─────────────────────────────────────────────────────────────┘
```

```
┌─────────────────────────────────────────────────────────────┐
│ Step 2 of 5 — Server                                        │
├─────────────────────────────────────────────────────────────┤
│ Where does this agent enroll?                               │
│                                                             │
│  Server URL: [ https://roomler.ai            ]              │
│                                                             │
│  Device name (optional):                                    │
│              [ PC50045 — Reception                  ]       │
│                                                             │
│   [ Back ]                                    [ Continue ]  │
└─────────────────────────────────────────────────────────────┘
```

```
┌─────────────────────────────────────────────────────────────┐
│ Step 3 of 5 — Enrollment token                              │
├─────────────────────────────────────────────────────────────┤
│ Paste a fresh enrollment token from the Roomler admin UI:   │
│                                                             │
│  Token: [ eyJhbGc...                                  ]     │
│         [ Use a token I already saved on this machine ]     │
│                                                             │
│   [ Back ]                                    [ Continue ]  │
└─────────────────────────────────────────────────────────────┘
```

If the operator picked "Use existing", read the token from the config and display its issuer + expiry so they can confirm before proceeding.

```
┌─────────────────────────────────────────────────────────────┐
│ Step 4 of 5 — Installing                                    │
├─────────────────────────────────────────────────────────────┤
│ ✓ Uninstalled previous version (0.3.0-rc.25 perMachine)     │
│ ✓ Installed roomler-agent 0.3.0-rc.26 to C:\Program Files\  │
│ ✓ Registered Windows service (RoomlerAgentService)          │
│ ✓ Enabled SystemContext mode (ROOMLER_AGENT_ENABLE_SYSTEM_  │
│   SWAP=1 set on the service environment)                    │
│ ✓ Started service                                           │
│ ⏳ Enrolling agent against https://roomler.ai...            │
│                                                             │
│   [ Cancel ]                                                │
└─────────────────────────────────────────────────────────────┘
```

```
┌─────────────────────────────────────────────────────────────┐
│ Step 5 of 5 — Done                                          │
├─────────────────────────────────────────────────────────────┤
│ ✓ Roomler Agent 0.3.0-rc.26 is running.                     │
│                                                             │
│ Agent ID:      69f3771d9fc07b0c99e476f8                     │
│ Machine name:  PC50045 — Reception                          │
│ Connected as:  LocalSystem (SystemContext / A1-path)        │
│                                                             │
│ Open your browser to:                                       │
│   https://roomler.ai/tenant/.../agent/.../remote            │
│   [ Open in browser ]                                       │
│                                                             │
│                                                  [ Finish ] │
└─────────────────────────────────────────────────────────────┘
```

### Implementation plan (rc.27 cycle)

| Task | Files | Effort |
|---|---|---|
| 1. Wizard window in Tauri | `agents/roomler-agent-tray/src/front/wizard.html` (new), `src-tauri/src/lib.rs` invoke handlers | 4 h |
| 2. Detect existing install (registry + service query + cargo-wix bundle metadata) | `agents/roomler-agent/src/install_detect.rs` (new lib helper) | 2 h |
| 3. Detect existing token + config | reuse `crate::config::load`, expose `read_jwt_issuer_and_exp()` helper | 1 h |
| 4. Mode-aware installation (call msiexec with correct MSI + post-install steps) | `agents/roomler-agent-tray/src-tauri/src/install.rs` (new) | 4 h |
| 5. Service env-var registration (`reg add`) without operator PowerShell | wrap `RegSetValueEx` via `windows-sys` | 1 h |
| 6. Enrollment call from wizard (reuse `enrollment::enroll`) | already exists | 0.5 h |
| 7. Old-version uninstall via stored `ProductCode` | enumerate via WMIC or use `msiexec /x{GUID}` from registry detect | 2 h |
| 8. Release pipeline: bundle wizard as `roomler-installer-x86_64.exe` alongside the MSIs | `.github/workflows/release-agent.yml` | 2 h |
| 9. Smoke test the wizard end-to-end on a fresh Win11 VM | manual | 1 h |
| 10. Document the new install flow in `packaging/windows/README.txt` + landing page | docs | 1 h |

**Total**: ~18 h of focused engineering. Realistically one full session + part of a second.

### Open design questions for next session

1. **Single wizard vs two MSIs?** Could keep both MSIs and just have the wizard pick the right one, OR consolidate to one MSI that decides mode at runtime via wizard input. Recommendation: **keep two MSIs**, wizard picks (less divergence from current state).
2. **Update path for already-enrolled agents** — wizard should detect + preserve token + machine_id, then trigger the existing auto-updater rather than a fresh install. Less code, more reliable.
3. **Re-enrollment vs first enrollment** — token in config may have expired; wizard needs to handle "I see a token but it's expired" gracefully.
4. **macOS / Linux scope** — wizard ships only for Windows? Or expand to cross-platform via Tauri? Recommendation: **Windows first**, since 95%+ of field installs are Windows. Linux/macOS still use `.deb` / `.pkg` + CLI enroll.

## How to pick up next session

1. Read this file first.
2. Read `agents/roomler-agent-tray/` to understand the current onboarding GUI surface.
3. Confirm with the user: wizard design above acceptable? Any UX flow changes wanted before implementation?
4. If acceptable → start with Task 1 (wizard window scaffolding) since that's the highest-uncertainty piece. Tasks 2-9 are mechanical once the UI shell is in place.
5. Cut rc.27 once the wizard is functional. Smoke on PC50045 via a fresh-install simulation (uninstall first, then run the wizard).

## What's left in current task list (#166, #176)

- `#166 [pending] rc.19 P8 manual smoke on PC50045` — supersedable; rc.23 hotfix #3 + rc.26 deployment already exercised the upload + lock-screen flows. Mark completed.
- `#176 [pending] rc.23 Phase 3: E2E test against PC50045 via PROD roomler.ai` — keep open. Could become a follow-on after the wizard ships (E2E for the full install + first session flow).

## Memory updates needed

Add three entries (will be done in the next-session prep):

- **`reference_sctp_chunk_size.md`**: webrtc-rs SCTP `max_message_size = 65536` by default; data-channel chunks at OR near 64 KiB intermittently fail with `failed to handle_inbound: ErrChunk` silent-drop. Always cap browser-side chunks at 16 KiB for 4× margin. Validated PC50045 2026-05-13: 32 KB worked, 1 MB failed, 16 KiB chunks fixed.
- **`project_m3_a1_shipped.md`**: rc.26 enabled the A1-path under SystemContext on PC50045. The two gates are `is_system_context_worker()`-conditional. Operator can drive Winlogon + UAC + elevated apps from the browser when `ROOMLER_AGENT_ENABLE_SYSTEM_SWAP=1` is set on the SCM service environment.
- **`feedback_diagnostic_logs_before_speculative_fix.md`**: this cycle's lesson — speculative fixes (rc.19/rc.21/rc.22 staging strategies) didn't address the actual SCTP-boundary bug. Adding visibility (console traces + agent log streaming + tracing on new code paths) closed the diagnosis loop in one PC50045 round-trip and pointed at the real root cause. Default to "ship diagnostic logging WITH any speculative fix" rather than "ship fix, then add logs if it fails."
