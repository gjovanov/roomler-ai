# HANDOVER22 — end-of-rc.28 cycle (install wizard) + pending W10 smoke

> The rc.27 / rc.28 cycle delivers a Tauri 2 install + onboarding
> wizard for Windows that eliminates the copy-paste-PowerShell ritual
> from the operator's setup flow. rc.27 shipped the foundational lib
> (registry probe + JWT introspect + SCM env-var helpers + roomler.ai
> MSI proxy). rc.28 stacks the wizard EXE on top. **All code is in
> master + CI-validated**; the only step left before tagging
> `agent-v0.3.0-rc.28` is the manual smoke (S1–S10) on a fresh Win11
> VM.

## Cycle ship status

| Phase | What shipped | Status |
|---|---|---|
| rc.27 P0 | `updater.rs` visibility lifts (5 fns `pub(crate)/private → pub`) | ✓ master |
| rc.27 P1a | `install_detect::upgrade_codes` + WiX parity test | ✓ |
| rc.27 P1b | `install_detect::msi_guid` pack/unpack (compressed-GUID encoder) | ✓ |
| rc.27 P1c | `install_detect::detect_existing_install()` + registry probe | ✓ |
| rc.27 P2 | `jwt_introspect::parse_unverified` (never echoes token) | ✓ |
| rc.27 P3 | `win_service::environment` REG_MULTI_SZ helpers + `restart_service` | ✓ |
| rc.27 P4 | Backend `/api/agent/installer/{flavour}` + `/health` proxy | ✓ |
| **rc.28 W0+W1** | `agents/roomler-installer/` workspace member + Tauri entry + single-instance | ✓ |
| **rc.28 W2** | Wizard state persistence to `%LOCALAPPDATA%\…\wizard-state.json` | ✓ |
| **rc.28 W3** | `msi_runner` Win32 wait + MSI exit-code decode (+ live smoke) | ✓ |
| **rc.28 W4** | `asset_resolver` via roomler.ai proxy + SHA256 verify | ✓ |
| **rc.28 W5** | `progress::ProgressEvent` + replay log | ✓ |
| **rc.28 W6a** | 6 read-only invoke handlers (detect, state, validate-token) | ✓ |
| **rc.28 W6b** | `cmd_install` orchestrator + cancel + force-kill + replay command | ✓ |
| **rc.28 W7+W8** | Front-end SPA (1,173 LOC vanilla JS — index.html + styles.css + app.js) | ✓ |
| **rc.28 W9** | `release-agent.yml` builds + Authenticode-signs installer EXE | ✓ |
| rc.28 W10 | Manual smoke S1–S10 on fresh Win11 VM | **pending — operator task** |
| rc.28 W11 | Tag `agent-v0.3.0-rc.28` + CLAUDE.md status block (THIS doc) | pending |

**Tagged on GitHub**: `agent-v0.3.0-rc.22` … `agent-v0.3.0-rc.26`. **rc.27 + rc.28 commits are in master but NOT yet tagged** — the release workflow is gated on the operator running the W10 smoke first.

## What the wizard does

Operator double-clicks `roomler-installer-0.3.0-rc.28-x86_64-pc-windows-msvc.exe`. Five steps:

1. **Welcome** — auto-runs `cmd_detect_install` (registry probe). Shows "Detected: perMachine 0.3.0-rc.26" (or "Clean" / "Ambiguous"). Operator picks deployment mode from 3 radio cards:
   - perUser (no admin)
   - perMachine (UAC prompt at install)
   - perMachine + SystemContext (perMachine + lock-screen-drive capability — **v1 limitation**: SystemContext mode currently requires a manual elevated PowerShell follow-up; the wizard surfaces the exact commands on the Done page).
   Cross-flavour switch banner (peruser↔permachine) appears with an "I understand my enrollment will be lost" ack checkbox gating the Continue button (BLOCKER-7 fix).
2. **Server + device** — pre-filled `https://roomler.ai` + hostname. Edit if needed.
3. **Token** — paste enrollment token in a password-type input. 350 ms debounce → `cmd_validate_token` shows "Valid token. Issuer: roomler-ai; expires in 8 minutes." or expired/invalid banner. Token kept in memory only.
4. **Install** — orchestrator drives the pipeline:
   - Resolve installer via `roomler.ai/api/agent/installer/{flavour}/health`
   - Stream MSI bytes from `roomler.ai/api/agent/installer/{flavour}` to `%TEMP%`
   - Verify SHA256
   - Spawn msiexec (`ShellExecuteExW + verb=runas` for perMachine → UAC prompt)
   - Wait + decode MSI exit code (Success / UserCancel / FatalError / AnotherInstall / RebootRequired)
   - Enroll the agent via the existing `/api/agent/enroll`
   - Save `config.toml`
   Live ProgressEvent stream renders as a checklist with a download progress bar.
5. **Done** — shows agent_id, tenant_id, flavour, tag. SystemContext mode shows the PowerShell follow-up snippet inline.

Recovery panel (cog icon top-right) on every step: "Restart wizard from Welcome" clears state + persists fresh + jumps back. Snackbar surfaces transient messages including "Wizard is already running" (B9 — single-instance plugin's mid-install relaunch handling).

## Pending — W10 manual smoke on a fresh Win11 VM

The wizard EXE has NEVER been launched end-to-end. CI tests the lib (42 unit tests including live Win32 smoke of `MsiRunner`) but doesn't run the Tauri webview. The operator needs to:

1. Boot a clean Win11 24H2 VM (or use a fresh user account on PC50045).
2. Download `roomler-installer-0.3.0-rc.28-x86_64-pc-windows-msvc.exe` from the agent-v0.3.0-rc.28 GitHub Release (once tagged) — or build locally: `cargo build -p roomler-installer --release` and run `target/release/roomler-installer.exe`.
3. Run the 10 smoke scenarios. Per the plan:
   - **S1** clean install perUser, fresh token
   - **S2** clean install perMachine + SystemContext (verify Done page surfaces the manual PowerShell follow-up snippet)
   - **S3a** same-flavour upgrade (perUser → perUser; machine_id must be preserved)
   - **S3b** cross-flavour switch (perUser → perMachine; expect "enrollment lost" warning + checkbox gate; new machine_id after)
   - **S4** re-enrol with fresh token on already-installed host
   - **S5a** pre-spawn cancel (click Cancel on Step 3 → returns to Welcome cleanly)
   - **S5b** post-spawn force-kill (msiexec running → click Cancel → confirm dialog → TerminateProcess; partial install warning)
   - **S6** UAC declined on perMachine (operator clicks No on UAC prompt → wizard shows "Installation cancelled (UAC declined)")
   - **S7** network unreachable during enroll (disconnect mid-install → wizard shows clear error)
   - **S8** two pre-existing flavours residue cleanup (if rc.18 MSIs still available — see HANDOVER21 H3)
   - **S9** single-instance: launch wizard EXE twice during in-flight install → second launch surfaces "Wizard is already running" snackbar on existing window
   - **S10** resume mid-flight: force-close wizard at Step 3, relaunch → wizard resumes on Step 3 with device-name + flavour pre-filled (token field empty — never persisted)

Auto-fail conditions (per the plan): machine_id changes across S3a; token leaked to `wizard-state.json` (grep `cat wizard-state.json | grep "agent-token"` — should match nothing); wizard EXE > 30 MB (currently 14.4 MB — well within); etc.

## ⚠️ BLOCKER for production-target smoke — rc.27 backend NOT deployed to prod

Verified 2026-05-15 by `curl https://roomler.ai/api/agent/installer/peruser/health` → **HTTP 404**. Prod `roomler.ai/health` reports `0.3.0-rc.24`; rc.27 + rc.28 are in master but never tagged/deployed. **The wizard's `cmd_install` will fail in field smoke against the production backend until rc.27 ships to prod.**

Two paths forward — pick one BEFORE running W10:

**Option A — deploy rc.27 backend to prod first** (recommended for honest field smoke):
1. SSH to mars (existing tmux pattern per `reference_deploy_workflow.md` + `reference_tmux_mars2.md`).
2. Build + push `registry.roomler.ai/roomler-ai:<new-tag>`.
3. Bump tag in `/home/gjovanov/roomler-ai-deploy/k8s/overlays/prod/kustomization.yaml`, git push.
4. ArgoCD webhook auto-reconciles in ~5s.
5. Verify `curl https://roomler.ai/api/agent/installer/peruser/health` returns valid JSON with `digest: "sha256:..."`.
6. Then run W10 smoke against prod.

**Option B — run W10 smoke against a staging/local backend first**:
- Set `ROOMLER_INSTALLER_PROXY_BASE=https://staging.roomler.ai/api/agent/installer` (or your local `cargo run -p roomler-ai-api` instance at `http://localhost:3000/api/agent/installer`) before launching the wizard EXE.
- This validates the wizard end-to-end without touching prod, but the released installer EXE will still need rc.27 deployed before it works in the field.

Option A is the right ship order. Note that the rc.27 backend changes are backwards-compatible — the new `/installer/{flavour}` route doesn't disturb existing `/latest-release` consumers (the agent's auto-updater still works). So deploying rc.27 to prod is low-risk independent of the wizard cycle.

## Local release build confirmation

Built locally on Win11 MSVC during the rc.28 W9 cycle: `target/release/roomler-installer.exe` produced, 14.4 MB. Compiles cleanly with the full Tauri 2 runtime + custom-protocol bundled assets. `cargo test -p roomler-installer --lib` → 42 / 42 green (7 msi_runner + 5 progress + 7 wizard_state + 11 asset_resolver + 12 install_orchestrator).

## Open items NOT in rc.28

- **SystemContext mode automation** — wizard v1 returns a clear "use the CLI" error when the operator picks `permachine-system-context`. Full automatic SCM env-var write + service restart needs a clean self-elevation path that doesn't surface a second UAC prompt mid-flow. Deferred to **rc.29**.
- **macOS / Linux wizard parity** — wizard is Windows-only for v1. Linux + macOS still use `.deb` / `.pkg` + CLI enroll. The lib pieces (`jwt_introspect`, the `directories`-based wizard state) compile cross-platform; only the `msi_runner` + flavour logic + asset resolver are Windows-shaped.
- **HANDOVER18 Bug 2 (admin pwsh input)** — still task #178. Independent of the wizard cycle.

## How to pick up next session

1. Read this file first.
2. **Operator runs W10 smoke** on a fresh Win11 VM (or PC50045).
3. If smoke green: tag `agent-v0.3.0-rc.28` to trigger `release-agent.yml`. Verify the 4 Windows artifacts publish (perUser MSI, perMachine MSI, tray EXE, installer EXE — all Authenticode-signed if the secret is set).
4. If smoke fails: capture the failure in a follow-up commit, re-run smoke, iterate.
5. Distribute the installer EXE URL to operators via the admin UI. The "download the wizard" CTA replaces the "download the right MSI" CTA in the existing flow.

## Memory updates planned

- New entry: rc.28 install wizard shipped; lib name is `wizard_core` (NOT `roomler_installer`) to dodge Windows UAC installer-detection heuristic.
- Update `project_rc27_foundation.md` to point at this file as the rc.28 ship target.
