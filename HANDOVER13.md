# Handover #13 — 0.2.7 hotfix shipped + M3 A1 implementation 70% landed

> Continuation of HANDOVER12 (which closed at the 0.2.6 asset-picker
> fix + M3 A1 pre-flight phase complete + P0 input regression
> diagnosed). This window: shipped 0.2.7 P0 hotfix, then landed 5 of
> 5 M3 A1 primitive modules + UI surface + perMachine MSI registry +
> supervisor rename + spawn-arm runtime wiring + verification harness.
> What's left is the worker-internal pipeline (capture pump + input
> pump) plus the heartbeat that triggers the spawn arm at runtime,
> plus the release CI flag flip and the 0.3.0-rc1 cut.

## Releases shipped

| Tag | Substance | CI |
|---|---|---|
| `agent-v0.2.7` | P0 input regression hotfix. `OpenInputDesktop` was called with `GENERIC_READ \| DESKTOP_SWITCHDESKTOP`; the latter requires `SE_TCB_NAME` (reserved for SYSTEM/LocalService/NetworkService) which non-SYSTEM callers including admins don't have. Every user-context agent on 0.2.0–0.2.6 false-`Locked` permanently → input dropped + lock-overlay frame substituted. Field repro PC50045 / e069019l 2026-05-04. Fix: drop the privilege at all 3 call sites + 2 regression tests via `concat!`-built needle. **194 → 197 lib tests** (+2 regression guards). | green, both MSIs published 2026-05-04 22:42 UTC |

The 0.3.0-rc1 cut is queued as the SECOND big action of the next
session (the FIRST is finishing the M3 A1 worker-internal wiring).

## M3 A1 implementation progress — 5 of 5 primitives + 6 of 9 wiring tasks

The M3 A1 plan (`~/.claude/plans/floating-splashing-nebula.md`,
approved 2026-05-03 critique-incorporated) breaks down into:
**6 pre-flight verifications** + **5 primitive modules** + **wiring
into existing supervisor / capture-pump / input / browser / CI**.

Pre-flights: 5 of 6 done last cycle (4 confirmed, 1 skipped as
unnecessary, 1 deferred to "block-before-promotion-not-implementation").
This cycle starts with all primitives as the next phase.

| # | Module / wiring item | Commit | Tests |
|---|---|---|---|
| 1 | `system_context` Cargo feature + `worker_role::probe_self` (token SID classifier) | `f032240` | +9 |
| 2 | `winlogon_token`: full spawn pipeline + RAII + build_cmdline + `into_raw_parts` | `34fd148` | +14 |
| 3 | `desktop_rebind`: `attach_to_winsta0`, `try_change_desktop`, DesktopChange enum | `61fb5d5` | +5 |
| 4 | `dxgi_dup`: 5-variant BackendBail + DxgiDupBackend with primary/frame/reset | `926fdc2` | +13 |
| 5 | `gdi_backend`: BitBlt-from-desktop-DC fallback after 3 consecutive DXGI HardError | `f68a8f8` | +5 |
| 6 | UI: `rc:desktop_changed` parse + `currentDesktop` ref + `RemoteControl.vue` "On Winlogon" chip | `c3019a5` | +5 frontend |
| 7 | `wix-perMachine/main.wxs`: `SoftwareSASGeneration=1` registry write (Critique #12 — without it Ctrl+Alt+Del from browser silently fails on lock screen) | `45097df` | — |
| 8 | Supervisor rename: `SystemContextCapture` → `SpawnSystemInSession(u32)` carrying target session id; `decide_spawn` 4th arg `last_active_session: Option<u32>` | `89a1386` | (rewritten) |
| 9 | Supervisor spawn-arm runtime wiring: composes `winlogon_token::find_winlogon_pid_in_session → open_winlogon_primary_token → spawn_system_in_session`; bridges ChildHandle → OwnedProcess via new `into_raw_parts` + `from_raw_parts` | `3f8521f` | — |
| 10 | `m3-a1-verify-win11.ps1` operator harness (Status / Install / SystemSpawn / LockUnlockCycle / Latency / Logs / Rollback) | `41c1d66` | — |

**Test surface 2026-05-05 EOD:** `192 → 268` lib tests with
`--features full-hw,system-context` (+76 absolute). Frontend
useRemoteControl: `117 → 122` (+5). Default agent build unchanged
at `194`; perUser MSI feature set (`--features full`) compiles
unchanged with no warnings. All 11 commits listed above are on
master and pushed to `github.com/gjovanov/roomler-ai`.

**Architecture is GO and the spawn site is wired.** The supervisor's
`SpawnSystemInSession(sid)` arm now actually spawns a SYSTEM-in-
session-N worker via the winlogon-token primitives. What's missing
is (a) the worker probing its role and switching its capture/input
plumbing accordingly, and (b) the runtime trigger that makes the
supervisor decide to fire the new spawn arm.

## What's queued for next session (in order)

The remaining M3 A1 work is `~3 + 1 + 1` commits:

### 3 worker-internal commits

1. **`peer.rs::media_pump` `CaptureSource` trait.** The biggest
   remaining refactor. Today the capture pump is hardcoded to
   call into `capture::scrap_backend` / `capture::wgc_backend`.
   M3 A1 needs a third path: `system_context::dxgi_dup` →
   `system_context::gdi_backend` after 3 consecutive HardError
   (RustDesk's `video_service.rs:851-856` trip-wire convention).
   The cleanest shape: a `CaptureSource` trait with `frame() ->
   Result<Frame, BackendBail>` + `reset()` + `dimensions()`,
   implemented by `UserModeCapture` (today's wrapping of WGC /
   scrap) and `SystemContextCapture` (DXGI + GDI fallback). The
   pump consumes the trait; the worker constructs the right
   impl based on `worker_role::probe_self()` at startup.
   Stale-frame detection rewritten to count consecutive
   `Ok(None)` returns (Critique §1 point A: monotonic-frame
   equality can't fire for `scrap`). Heartbeat task writes one
   byte every 5 s to the inherited pipe while
   `pc.connection_state() == Connected`.

2. **`enigo_backend.rs` split.** Today's enigo backend runs as a
   tokio task; that breaks under M3 A1 because `SetThreadDesktop`
   is per-thread and tokio's work-stealing executor distributes
   tasks non-deterministically. Split into `UserModeBackend`
   (today's behaviour, rename from current) and
   `SystemContextBackend` (single dedicated `std::thread` that
   owns the desktop binding, channel-fed from tokio runtime via
   `tokio::sync::mpsc`, calls
   `desktop_rebind::try_change_desktop()` before every
   `SendInput`). The per-thread binding constraint forbids the
   prelude-on-every-inject pattern in the original draft.

3. **Heartbeat-pipe wiring.** Without this the supervisor's
   `SpawnSystemInSession(sid)` arm at `3f8521f` is unreachable
   at runtime (the arm fires only when `keep_stream_alive=true`,
   which is currently always passed `false`). API change to
   `winlogon_token::spawn_system_in_session`: accept an optional
   inheritable handle for the heartbeat-pipe write end, set
   `STARTUPINFOEX::lpAttributeList` with
   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` carrying it. Supervisor
   creates the pipe, spawns the worker with the write end
   inherited, polls the read end on the supervisor thread; one
   byte every 5 s while controller-connected → flips
   `keep_stream_alive` to true. Stops arriving → flips back to
   false.

### 1 release CI commit

4. **`.github/workflows/release-agent.yml`** — perMachine MSI build
   adds `--features full-hw,system-context`. perUser unchanged.
   **One-line edit but MUST BE LAST** because it flips the feature
   ON for end users. Until this lands, all the dormant M3 A1 code
   compiles into the dev-only `cargo build --features full-hw,
   system-context` binary but never makes it into a release MSI.

### 1 release tag commit

5. **Cut `0.3.0-rc1`** — bump workspace version 0.2.7 → 0.3.0-rc1,
   tag `agent-v0.3.0-rc1`, push. Run `m3-a1-verify-win11.ps1` on
   PC50045 once CI publishes:
   * `-Action Install` (elevated) → install the perMachine 0.3.0-rc1 MSI
   * `-Action Status` → confirm SCM service running, SoftwareSASGeneration=1
   * Connect from a browser controller; `-Action SystemSpawn` →
     verify ≥1 worker as S-1-5-18 in non-zero session
   * `-Action LockUnlockCycle` → manual operator path through 5
     Win+L / unlock cycles
   * `-Action Latency` → extract M3 A1 transition events from the
     supervisor log; correlate with browser-side rVFC
     timestamps for visual-latency p50 / p99
   * Promote `0.3.0-rc1` → `0.3.0` if all gates pass per the plan
     (Task #119)

### Pre-flight #4 (RDP-only host) before promotion

The plan calls for confirming `WTSEnumerateSessions` vs
`WTSGetActiveConsoleSessionId` on an RDP-only host before the
0.3.0 promotion. Architecture is decided (RustDesk's pattern +
our pre-flights converge on `WTSEnumerateSessions`); #4 is a
confirm-on-real-hardware step. Provision an Azure / Hetzner
RDP-only VM during the 0.3.0-rc field-smoke window.

## Sharp edges discovered + closed this cycle

* **0.2.7 hotfix found in test code itself**: the new regression
  test scanning for `DESKTOP_SWITCHDESKTOP` initially included the
  literal in its own assert messages → trivially failed itself.
  Fixed by building the search needle via `concat!("| ",
  "DESKTOP_SWITCH", "DESKTOP")` so the source-text only contains
  the SPLIT components. Documented inline.

* **Pre-existing fmt drift in `system_context_probe.rs`** kept
  showing up as collateral damage when running `cargo fmt` on
  newly-added M3 A1 modules. Reverted incidental edits in each
  commit so the M3 A1 commits stayed focused; the 9 fmt diffs
  in `system_context_probe.rs` (from `73df8d8`) are still on
  master uncorrected. Whoever cuts 0.3.0-rc1 should consider
  formatting the file as a chore commit so CI's
  `cargo fmt --check` stays green if it ever runs against that
  file.

* **Pre-existing clippy warning in `updater.rs:248`** (`needless
  return` inside a `#[cfg(target_os = "windows")]` block, latent
  since `4c89e5b` / 0.2.6) — only flags on Windows, only with
  `--features full`, and the CI workflow `ci.yml` runs
  `clippy --workspace` (no features) which gates it out. Not
  fixed in this cycle. Worth a one-line drop of the `return`
  before the next hotfix that touches that file.

* **`OwnedProcess::from_raw_parts` dead-code under `--features
  full`**: needed to gate the constructor `#[cfg(feature =
  "system-context")]` because the only caller is the M3 A1 spawn
  arm. Without the gate, `cargo check --features full` raises
  `dead_code` and (under `-D warnings`) fails. Confirmed CI
  `release-agent.yml` doesn't run `clippy -D warnings` so this
  wouldn't have shipped a real failure — but we caught it locally
  and gated it cleanly.

* **`CommandLineToArgvW` escape semantics in `winlogon_token::
  build_cmdline`**: tests cover the four edge cases (space, tab,
  embedded quote, trailing backslash) plus empty arg + no-args
  baseline. Expect zero argv parse drift on the receiving
  `roomler-agent.exe` regardless of path.

* **Pipe `0xFFFFFFFF` from `WTSGetActiveConsoleSessionId` on
  RDP-only hosts** (the basis for using `WTSEnumerateSessions`
  instead — RustDesk lesson, our Pre-flight #4 is the empirical
  confirm) is documented in the `winlogon_token::find_active_
  session` module header. Pre-flight #4 must run before 0.3.0
  promotion regardless.

## State at end of session

* master HEAD: `41c1d66` (`m3-a1-verify-win11.ps1` script)
* Latest GitHub Release: `agent-v0.2.7` published 2026-05-04 22:42 UTC
* agent-lib tests: 268 with `--features full-hw,system-context`
  (was 192 at session start, +76); 194 default unchanged
* Frontend tests: 122 useRemoteControl (+5 desktop_changed
  variants); 357 total (was 352)
* M3 A1 plan: `~/.claude/plans/floating-splashing-nebula.md`
* M3 A1 implementation tracker: `memory/project_m3_a1_implementation.md`
* Operator harness: `agents/roomler-agent/scripts/m3-a1-verify-win11.ps1`

## Useful artefacts for next session

* **Plan file**: `~/.claude/plans/floating-splashing-nebula.md`
  (M3 A1 implementation plan, single 0.3.0-rc cut behind
  `system-context` feature)
* **Implementation tracker**: `memory/project_m3_a1_implementation.md`
  (commit-by-commit progress log; updated each session)
* **Pre-flight memories**:
  * `memory/project_m3_a1_rustdesk_finding.md` — architectural inspiration
  * `memory/project_m3_a1_preflight_1_scrap.md` — BackendBail discrimination table
  * `memory/project_m3_a1_preflights_2_3_5.md` — empirical privilege + cadence results
* **Hotfix brief**: `memory/project_input_regression_0_2_x.md`
  — full diagnosis of the 0.2.0–0.2.6 P0 + the 0.2.7 fix shipped
  this session
* **Verification harness**: `agents/roomler-agent/scripts/m3-a1-verify-win11.ps1`
  ready for use once 0.3.0-rc1 ships
* **Probe binary** at `target/release/roomler-agent.exe` — keep
  handy for re-running #2/#3/#5 on hardware variations (e.g. the
  RDP-only VM for #4)
* **m5-verify-win11.ps1** at `agents/roomler-agent/scripts/` —
  the M2 path harness for cross-reference

## Resume checklist for the next session

1. Read this file (HANDOVER13.md) first.
2. Read `memory/project_m3_a1_implementation.md` for the commit-by-commit detail.
3. Read `memory/project_input_regression_0_2_x.md` if validating the 0.2.7 hotfix on PC50045.
4. Confirm 0.2.7 is in the field via `roomler-agent --version` on PC50045 (auto-update should have rolled forward by then).
5. Begin the `peer.rs::media_pump` `CaptureSource` trait refactor (the big remaining piece). The other two worker-internal commits can land in either order; the heartbeat pipe is best done BEFORE the release-agent.yml flag flip so the runtime trigger actually fires.
