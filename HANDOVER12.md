# Handover #12 — 0.2.6 → M3 A1 pre-flight phase complete + P0 input regression diagnosed

> Continuation of HANDOVER11 (which closed at 0.2.5 / Z-path /
> A1 NO-GO via WGC). This window: shipped 0.2.6 (asset-picker
> flavour-aware), validated the M3 A1 RustDesk-pattern architecture
> empirically via 5 of 6 pre-flight spikes, and discovered a P0
> input regression that bricks every user-context agent install
> 0.2.0 through 0.2.6 inclusive.

## Releases shipped

| Tag | Substance | CI |
|---|---|---|
| 0.2.6 | Auto-update asset picker becomes flavour-aware (`WindowsInstallFlavour` enum + `current_install_flavour()` reads `std::env::current_exe()`; perUser/perMachine MSI selection by `-perMachine-` infix). Field repro: PC50045 e069019l 2026-05-02, perUser agent on 0.2.0 picked the perMachine 0.2.5 MSI alphabetically; cross-flavour launch condition silently rejected; agent stuck at 0.2.0 forever. 9 new tests pin the contract. | green |

The 0.2.7 hotfix for the input regression (see below) is queued as the FIRST action of the next session.

## M3 A1 pre-flight phase — 5 of 6 done

The M3 A1 plan (in `~/.claude/plans/floating-splashing-nebula.md`) calls for 6 pre-flight spikes before any production code. Five passed empirically on PC50045 2026-05-04; #4 is deferred to "block before 0.3.0 promotion, not before implementation".

| # | Status | Result |
|---|---|---|
| #1 DXGI access-lost surface in pinned scrap | ✅ source-reading | scrap-0.5.0 wrapper translates `TimedOut → WouldBlock`; ACCESS_LOST → ConnectionReset. Plan's single `BackendBail::DesktopChanged` should split into 5 variants (Transient / DesktopMismatch / AccessLost / SessionGone / HardError). Memory: `project_m3_a1_preflight_1_scrap.md` |
| #2 Win11 24H2 token-cross-session permission | ✅ empirical | Bare `OpenProcessToken(winlogon.exe pid 1248)` + `DuplicateTokenEx(TokenPrimary)` + `SetTokenInformation(TokenSessionId=1)` + `CreateProcessAsUserW` spawned a child as `S-1-5-18` in session 1. **No `AdjustTokenPrivileges` needed.** SE_TCB + SE_IMPERSONATE enabled in child by default. SeAssignPrimaryTokenPrivilege DISABLED — flagged for future if we ever want sub-spawn from the SYSTEM worker. Critic's worry about Win11 24H2 LSA tightening is REFUTED on this hardware. |
| #3 SetProcessWindowStation requirement empirically | ✅ partial | `psexec -s -i 0` empirically drops the process directly into `WinSta0` (NOT the SCM's `Service-0x0-3e7$\Default`), so the failure-mode wasn't reproduced — pre-attach `OpenDesktopW("Winlogon")` already succeeded. Architectural conclusion stands: production SCM service starts in `Service-0x0-3e7$` and must call `attach_to_winsta0()` defensively at SYSTEM-context worker startup. RustDesk does the same. True ground truth comes during 0.3.0 implementation when the SCM hosts the worker spawn. |
| #4 WTSEnumerateSessions vs WTSGetActiveConsoleSessionId on RDP-only host | ⏸ deferred | Needs an RDP-only Azure/Hetzner VM. Doesn't change architecture (RustDesk uses WTSEnumerateSessions; we already do too in the probe). Block before 0.3.0 promotion, not before implementation. |
| #5 scrap DXGI frame timing | ✅ empirical | 30 s sample on PC50045 user desktop: 1789 iterations / 59.6 it/s wallclock, 31% Ok (small motion), 69% WouldBlock (TimedOut wrapped), 0% across all error variants. ~5 GiB Ok bytes. M3 A1 `media_pump` against DXGI can use the same idle-keepalive strategy as today's WGC path. No encoder pipeline timing surgery needed. |
| #6 cut 0.2.9-perMachine baseline | ✅ skipped | `gh release view agent-v0.2.6 --json assets` confirmed perMachine MSI already published. The flavour-aware picker shipped in 0.2.6 will correctly `pin_version("agent-v0.2.6") → perMachine` for rollback. No new release needed. |

Memory: `project_m3_a1_preflights_2_3_5.md` (full privilege table from the winlogon-token probe child + cadence histogram).

**Architecture is empirically GO.** RustDesk's pattern (DXGI Desktop Duplication + winlogon-token spawn + LocalSystem SCM service + lazy SetThreadDesktop rebind) replicates cleanly on this Win11 box without privilege adjustments, kernel drivers, or undocumented APIs.

## P0 input regression diagnosed (0.2.0 → 0.2.6, all versions affected)

User report: "0.2.6 has no working mouse and keyboard." Investigation root-caused this to a P0 bug introduced in 0.2.0 that affects every non-SYSTEM agent install across the 0.2.x series.

**Root cause.** `agents/roomler-agent/src/win_service/desktop.rs:88,114` — `OpenInputDesktop` / `OpenDesktopW` are called with access mask `GENERIC_READ | DESKTOP_SWITCHDESKTOP`. Per Microsoft docs, `DESKTOP_SWITCHDESKTOP` requires `SE_TCB_NAME` privilege, which is reserved for `LocalSystem` / `NetworkService` / `LocalService`. **Even Administrator accounts don't have SE_TCB_NAME.** The user-context probe in `lock_state.rs::probe_lock_state` always gets `ACCESS_DENIED` → returns `Locked` → state stays `Locked` permanently → input handler drops every event (peer.rs:1574) + capture pump substitutes the lock-overlay frame (peer.rs:937, :1400).

We never call `SwitchDesktop()` anywhere in the codebase. The flag was always wrong; we just never tested under a non-SYSTEM caller because every M3 SystemCaptureSmoke run was via `psexec -s -i 1` (SYSTEM context).

**Why M3 cycle missed it.** Memory `project_input_regression_0_2_x.md` has the full timeline. TL;DR: every test runner / spike binary in the 0.2.x cycle ran with SE_TCB_NAME present (via psexec or CI service context). The unit test `desktop::tests::open_input_desktop_works_or_denies_cleanly` accepts both `Some` and `None` as success; never caught the false-positive.

**The fix (next session, FIRST action — 0.2.7 hotfix).**

Three call sites, each gets the same one-line change (`GENERIC_READ | DESKTOP_SWITCHDESKTOP` → `GENERIC_READ`):

1. `agents/roomler-agent/src/win_service/desktop.rs:88` — `open_desktop_by_name`
2. `agents/roomler-agent/src/win_service/desktop.rs:114` — `open_input_desktop` (the regression source)
3. `agents/roomler-agent/src/win_service/system_context_probe.rs:520` — the WinSta-attach probe

Plus removing the `DESKTOP_SWITCHDESKTOP` import in both files.

Add a regression test in `desktop.rs::tests` using `include_str!("desktop.rs")` to scan for the literal `DESKTOP_SWITCHDESKTOP` near `OpenInputDesktop` / `OpenDesktopW` calls and fail compilation if it reappears.

**Z-path lock detection survives the fix** — Winlogon's DACL still denies non-SYSTEM regardless of mask, so true-positive lock detection keeps working.

**Field smoke** post-0.2.7:
1. Connect from `roomler.ai`. Confirm desktop renders (NOT yellow padlock overlay). No "Host locked" chip in toolbar.
2. Drag mouse / click / type → effects on host.
3. Press Win+L. Within ~500 ms: padlock overlay + chip appear, input dropped (correctly).
4. Unlock. Overlay vanishes, input flows.

Memory: `project_input_regression_0_2_x.md`.

## What's queued for next session (in order)

1. **0.2.7 hotfix**: drop `DESKTOP_SWITCHDESKTOP` from the 3 call sites + add regression test + cut tag. ~30 minutes including CI wait.
2. **Field-smoke 0.2.7 on PC50045** to confirm input flows again. Operator path documented in `project_input_regression_0_2_x.md`.
3. **Begin M3 A1 implementation** (Task #118): single 0.3.0-rc cut behind the `system-context` Cargo feature flag. Plan in `~/.claude/plans/floating-splashing-nebula.md`. Files to create: `winlogon_token.rs`, `desktop_rebind.rs`, `dxgi_dup.rs`, `gdi_backend.rs`, `worker_role.rs`. Files to modify: `supervisor.rs`, `peer.rs`, `enigo_backend.rs` (split User/SystemContext), `main.rs`, `updater.rs`, `wix-perMachine/main.wxs`, `signaling.rs`, `useRemoteControl.ts`, `RemoteControl.vue`, release-agent.yml.
4. **Cut 0.3.0-rc1**, run `m3-a1-verify-win11.ps1` on PC50045 (Status / Install / SystemSpawn / LockUnlockCycle / DesktopTransitionLatency / Logs actions — to be written; mirror existing `m5-verify-win11.ps1`). Acceptance: <1.5s p50, <3s p99 lock→unlock visual latency.
5. **Pre-flight #4** (RDP-only host) before 0.3.0 promotion. Needs an Azure/Hetzner VM provisioned.
6. **Promote to 0.3.0** if all gates pass.

## State at end of session

- master HEAD: `4c89e5b` (0.2.6 published)
- Latest GitHub Release: `agent-v0.2.6` 2026-05-02
- agent-lib tests: 192 (was 186 at session start; +6 from `system_context_probe::tests`)
- M3 A1 plan: `~/.claude/plans/floating-splashing-nebula.md` (approved 2026-05-03)
- Probe binary: `roomler-agent system-context-probe {winlogon-token | winsta-attach | dxgi-cadence}` — committed to master, gated on Windows + (for dxgi-cadence) `feature = "scrap-capture"`. Already deployed at `C:\Users\e069019l\AppData\Local\Programs\roomler-agent\roomler-agent.exe` on PC50045.

## Useful artefacts for next session

- **Plan file**: `~/.claude/plans/floating-splashing-nebula.md` (M3 A1 implementation plan, critique-incorporated, single 0.3.0-rc behind `system-context` feature)
- **Probe binary** at `target/release/roomler-agent.exe` — keep handy for re-running #2/#3/#5 on hardware variations (e.g. when an RDP-only host comes online for #4)
- **m5-verify-win11.ps1** at `agents/roomler-agent/scripts/m5-verify-win11.ps1` — model for the upcoming `m3-a1-verify-win11.ps1`
- **Memories** indexed in `~/.claude/projects/C--dev-gjovanov-roomler-ai/memory/MEMORY.md`:
  - `project_m3_a1_rustdesk_finding.md` — architectural inspiration
  - `project_m3_a1_preflight_1_scrap.md` — BackendBail discrimination table
  - `project_m3_a1_preflights_2_3_5.md` — empirical validation results
  - `project_input_regression_0_2_x.md` — the 0.2.7 hotfix brief
  - `feedback_powershell_heredoc_bat.md` — the heredoc-line-break trap (bit twice this cycle)

## Sharp edges discovered + closed

- **0.2.6 asset picker bug** (closed 0.2.6): GitHub release listing returns assets alphabetically; perMachine MSI sorted ahead of perUser; perUser agents on 0.2.0 polled and picked the wrong flavour; cross-flavour launch condition rejected silently; auto-update made zero progress. Fix: `WindowsInstallFlavour` enum, `current_install_flavour()` reads `std::env::current_exe()`, picker filters by `-perMachine-` infix.
- **PowerShell heredoc → .bat line-break trap** (closed via memory `feedback_powershell_heredoc_bat.md`): `@"..."@` preserves newlines; cmd.exe interprets a wrapped redirect as two commands; exit code 255 with no output file. Bit twice. Memory now permanent.
- **DESKTOP_SWITCHDESKTOP false-locks the user-context worker** (open, hotfix queued for next session).
- **psexec -s -i 0 lands in WinSta0 directly**, not Service-0x0-3e7$\Default. Means we can't use psexec to reproduce the SCM service's actual starting window-station state. Architectural conclusion (defensive `attach_to_winsta0()`) stands; true ground truth comes during 0.3.0 implementation.
- **Pre-flight #6 was unnecessary** — 0.2.6 already publishes a perMachine MSI; rollback target already exists.
