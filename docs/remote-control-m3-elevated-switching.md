# M3 elevated / user-app input switching — plan

**Status**: planning only (rc.22 cycle, 2026-05-12). No code in this RC. Implementation targeted for rc.23 once the bisect identifies the breaking commit.

**Field bug** (HANDOVER18 Bug 2): on PC50045 (Win11, perMachine MSI, e069019 account, SystemContext worker active), mouse input stops responding when the operator hovers an elevated/admin pwsh window. Memory `project_m3_a1_implementation.md` records that rc.7 *did* support input over admin apps via the LocalSystem (S-1-5-18) worker bypassing UIPI. Something regressed between rc.7 (2026-04-26) and rc.21 (2026-05-11).

## What we know

- SystemContext worker IS active on PC50045 — agent log lines confirm:
  ```
  INFO system-context capture: backend=DXGI
  INFO system-context input: thread already bound to input desktop at startup
  INFO input: backend=system-context (enigo with SetThreadDesktop rebind)
  ```
- The lock-state probe + suppression landed in rc.20 (Z-path). The probe runs from a tokio task and classifies any `current_thread_desktop_name() != "Default"` as `LockState::Locked`. When `Locked`, `attach_input_handler` drops every InputMsg before forwarding to `enigo`.
- The probe runs from a **tokio task** spawned by `lock_state::spawn_monitor` (peer.rs:590). The tokio runtime's worker threads inherit the process's initial desktop assignment.
- The SystemContext input thread is a dedicated thread that calls `SetThreadDesktop(Default)` at startup AND on every desktop transition. The lock_state probe task runs on a **different** tokio worker thread that may not have ever called `SetThreadDesktop`.

## Candidate regression points (bisect targets)

Per HANDOVER18 + recent commit history. Suspect order — most likely first:

### 1. rc.20 lock_state probe runs on the wrong thread
- `peer.rs::attach_input_handler` reads `lock_state_rx.borrow()` and short-circuits when `Locked`. The probe task's `current_thread_desktop_name()` reads the calling tokio thread's desktop — not the input desktop globally.
- Under the SystemContext worker, the agent process's main thread is the one explicitly bound to `Default`. Tokio worker threads inherit the process's INITIAL desktop binding. For a SystemContext worker spawned via `CreateProcessAsUserW` with winlogon-token + `STARTUPINFO.lpDesktop = "winsta0\\Default"`, the inherited desktop is `Default`. So fresh tokio threads SHOULD report `Default`.
- **But**: if any code on a tokio worker thread calls `SetThreadDesktop(Winlogon)` to reach Winlogon for capture/input during a lock screen and that thread is later reused by the probe task, the probe sees `Winlogon` → classifies as `Locked` → suppresses input even AFTER the user has unlocked.
- **Hypothesis**: the input thread's `SetThreadDesktop` rebind is leaking into the tokio worker pool. The fix would be to (a) bind the probe task to `Default` explicitly on every iteration, OR (b) make the probe read the **input desktop's** name via `OpenInputDesktop` + `desktop_name_of`, not the calling thread's desktop. (b) is more correct — what we care about is "is the input desktop locked", not "what desktop is the probe attached to".

### 2. 0.2.7 access-mask reduction
- `9fc63c3` dropped `DESKTOP_SWITCHDESKTOP` from `open_input_desktop` and `system_context_probe.rs`. The reduction targeted user-context probe paths (per memory `project_input_regression_0_2_x.md`) to avoid false-positive `Locked` on user accounts that lack `GENERIC_WRITE` on Winlogon's DACL.
- The reduction did NOT touch `open_input_desktop_for_injection` (which is the path the input thread takes). But the input thread's rebind path uses `set_thread_desktop` against the FOR_INJECTION handle, which carries `GENERIC_READ | GENERIC_WRITE`. Should still work.
- **Unlikely culprit on its own**, but worth re-verifying that `set_thread_desktop` against a `GENERIC_READ | GENERIC_WRITE` Default handle gives a thread enough rights for `SendInput` over an elevated window.

### 3. rc.16-rc.18 SetThreadDesktop rebind logic
- The input thread's "rebind on every desktop transition" loop lives in `agents/roomler-agent/src/input/win_input.rs` (or similar — verify path). If between rc.16 and rc.18 the rebind path lost the "rebind back to Default after Winlogon" step, then after a Win+L + unlock sequence the input thread stays bound to Winlogon.
- That would cause: admin pwsh input fails (input goes to Winlogon desktop, not Default where the admin pwsh window lives), but ordinary user-app input ALSO fails (same reason). The field report is "admin pwsh fails, ordinary input works" — which doesn't match this hypothesis.
- **Unlikely** but bisect should still hit it.

### 4. SetThreadDesktop on the input thread isn't enough for elevated windows
- Even when the input thread is bound to `Default`, `SendInput` against an elevated window requires the calling process token to have the SeTcbPrivilege (which LocalSystem has) AND the desktop session-attached integrity level to match-or-exceed the target. The desktop's IL is `System` for SystemContext workers — should bypass UIPI.
- **Unlikely** to be a regression specifically, but worth a sanity test under WinDbg / token inspector.

## Required pre-coding evidence

Before the next session touches code:

1. **Bisect rc.7 → rc.21 on PC50045**. Pull the per-User MSI for each of rc.7, rc.10, rc.13, rc.16, rc.18, rc.20, rc.21 from `github.com/gjovanov/roomler-ai/releases`. Install each in order; after each install:
   - Restart the agent (Scheduled Task or `roomler-agent run` from elevated shell).
   - Open an elevated pwsh (`Start-Process pwsh -Verb runas`).
   - Move the mouse over it from the browser viewer.
   - Type a character.
   - Log result + the agent log path snapshot at `C:\Windows\System32\config\systemprofile\AppData\Local\roomler\roomler-agent\data\logs\`.
2. The bisect's *first failing RC* narrows the suspect commits to a 24-commit window. Read those commits in detail before coding.
3. While bisecting, **also test rc.22 with `ROOMLER_AGENT_STAGING_LEGACY_PER_DEST=1`** set in the agent's env (via Scheduled Task XML or `setx`) so we can confirm rc.22 didn't inadvertently affect input.

## Proposed implementation shape (for rc.23, dependent on bisect)

Assuming hypothesis #1 (lock_state probe reads wrong thread's desktop) is confirmed:

### Change A: probe the input desktop, not the calling thread

`lock_state::probe_lock_state` currently calls:
```rust
match desktop::open_input_desktop() {
    Ok(Some(_d)) => {
        match desktop::current_thread_desktop_name() {   // ← wrong question
            Ok(name) => classify(true, &name),
            ...
```

Replace with:
```rust
match desktop::open_input_desktop() {
    Ok(Some(d)) => {
        match desktop::desktop_name_of(d.raw()) {        // ← right question
            Ok(name) => classify(true, &name),
            ...
```

This makes the probe correctly answer "what's the input desktop right now?" regardless of which tokio thread the probe lands on. The OwnedDesktop handle from `open_input_desktop` already has the desktop the probe wants; `desktop_name_of` just reads its name.

**Tests** (no-FFI test needed — desktop_name_of is already covered):
- Unit: classify still works the same; no test change.
- Manual: SystemContext worker on a Win11 box, lock screen + admin pwsh + Citrix scenarios.

### Change B: bind every fresh tokio worker that touches input to Default

If hypothesis #3 (rebind leak) bisects in:

`peer.rs::attach_input_handler` runs the `on_message` closure on whatever tokio worker thread picks up the DC's `MessageEvent`. Each closure invocation should:
```rust
#[cfg(all(feature = "system-context", target_os = "windows"))]
{
    let _ = crate::win_service::desktop::bind_thread_to_default_if_needed();
}
```

`bind_thread_to_default_if_needed` is a new helper: reads `current_thread_desktop_name()`, returns early if `"Default"`, else opens Default + calls `SetThreadDesktop`. Idempotent; cheap when already bound.

**Tests**:
- Unit: `bind_thread_to_default_if_needed` is a one-syscall-or-no-syscall helper; test the early-return path with a mock `current_thread_desktop_name`.
- Manual: same admin pwsh scenario on PC50045.

### Change C: split the suppression policy

The current "lock_state::Locked → drop input" policy is correct for the **Z-path** (operator can't drive the lock screen). It's WRONG for elevated apps after the worker switches to SystemContext — under SystemContext, input CAN drive elevated apps, so suppression shouldn't trigger just because the desktop transitions briefly to Winlogon (UAC consent).

Refine the policy:
```rust
// In attach_input_handler:
if matches!(*lock_state_rx.borrow(), lock_state::LockState::Locked) {
    // SystemContext worker bypasses UIPI — input CAN reach elevated
    // windows even when our desktop name says Winlogon (UAC consent
    // visible for ≤ 2 s). Only suppress when we're CERTAIN the
    // operator can't drive (true Win+L lock screen).
    #[cfg(all(feature = "system-context", target_os = "windows"))]
    if worker_role::probe_self() == Ok(WorkerRole::SystemContext) {
        // Probe whether the lock IS a Win+L lock (Logon UI visible)
        // vs a brief UAC consent. Only suppress for Win+L.
        if !is_winlogon_session() { return; }   // fall through to inject
    }
    // ...existing suppression path...
}
```

`is_winlogon_session()` is a new helper that calls `WTSQuerySessionInformation(SessionId, WTSSessionInfo)` and checks the `LogonState` field for `WTSLogonStateLoggedOff` or similar. Reliable signal for "user has logged out vs UAC is up".

**Tests**:
- Unit: mock `is_winlogon_session` returning true / false; assert correct dispatch.
- Manual: UAC consent visible mid-session should NOT suppress input; Win+L should.

## Risks

- Tokio worker thread reuse across desktops is undocumented behavior; behavior may shift across tokio versions. The right fix (Change A) sidesteps this entirely.
- `desktop_name_of` on a freshly-opened Winlogon handle from a non-SYSTEM context might fail; the SystemContext path always has SE_TCB, but the user-context fallback might not. Re-add fallback to `current_thread_desktop_name` if `desktop_name_of` errors.
- Change C (UAC consent suppression refinement) needs careful UX consideration — false-permitting input during a UAC prompt would let the operator click "Yes" remotely, which IS a security concern. The current "always-suppress" policy is conservatively safe; loosening it requires a deliberate decision per `docs/remote-control.md` §11.

## Tests to add post-fix

- Unit: `probe_lock_state` with mock `open_input_desktop` + `desktop_name_of` returning various combinations (success-with-Default, success-with-Winlogon, denied, error).
- Integration: simulate a desktop-transition watcher round-trip via the loopback peer harness; verify input flows correctly across the transition.
- Manual: PC50045 + e069019 + admin pwsh; verify keyboard + mouse work consistently across Win+L lock/unlock cycles.

## Rollout plan

1. Land Change A in rc.23 — it's the highest-confidence fix and doesn't touch the suppression policy.
2. Bisect on PC50045; if admin pwsh input still fails after rc.23, escalate to Change B.
3. If Change B doesn't resolve it, escalate to Change C, requiring an explicit UX decision about UAC-consent-prompt input policy.
