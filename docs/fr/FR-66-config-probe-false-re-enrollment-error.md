# FR-66: A healthy host is told to re-enroll, on every single service start

**Issue:** [#TBD](https://github.com/gjovanov/roomler-ai/issues) ·
**Status:** P0 — spec · **Owner:** agent/windows-service

## Goal

`roomlerd` must not log an operator-actionable ERROR that is false. Specifically:
a feature-flag probe that expects to find nothing, and correctly handles finding
nothing, must not emit `the host must be re-enrolled` about a host that is
enrolled, connected, and serving.

## Field evidence (neo16, 0.4.48, 2026-09-02)

Every service start logs this — three times on the day it was found:

```
ERROR config unreadable and no usable previous copy — the host must be re-enrolled
  path=C:\ProgramData\roomler\roomler\config.toml
  error=reading config at C:\ProgramData\roomler\roomler\config.toml:
        The system cannot find the file specified. (os error 2)
  prev=C:\ProgramData\roomler\roomler\config.toml.prev
  prev_error=... (os error 2)
```

The host it says that about:

```
● (this device)
  version     0.4.48        mode  service (SYSTEM)
  server      connected
  enrollments primary  connected, overlay tun (primary)
              jovanov  connected, overlay tun
```

Both enrollments up, overlay carrying traffic, config saved **that morning**.

### Why the file it names is legitimately absent

Three log lines apart, the same start says:

```
config: resolved load path
  config_path=C:\Windows\system32\config\systemprofile\...\roomler\config\config.toml
  is_system_context=true
  machine_global=C:\ProgramData\roomler\roomler\config.toml
supervisor: M3 A1 auto-swap (user-context -> SystemContext) is DISABLED (default)
supervisor: spawned worker  pid=109640  session_id=1  elevated=true
```

The worker runs **in session 1 as the elevated user**, so the config it actually
uses is `%APPDATA%\roomler\roomler\config\config.toml` — live, 3 670 bytes,
rewritten the same day, with a healthy same-second `.prev` beside it (the atomic
save working exactly as designed). The machine-global path is simply not this
install's topology. Nothing is wrong.

## Root cause

`agents/roomlerd/src/win_service/supervisor.rs:958` — `netd_enabled()`:

```rust
fn netd_enabled() -> bool {
    if let Some(v) = tunnel_core::env::node_env("OVERLAY_NETD") {
        return netd_flag_truthy(&v);
    }
    let p = crate::config::machine_global_config_path();
    crate::config::load(&p)
        .ok()
        .and_then(|c| c.overlay_netd)
        .unwrap_or(false)
}
```

Its **behaviour is correct**: `.ok()` + `unwrap_or(false)` means "absent ⇒ the
flag is off", which is right, and `overlay_netd` gates a scaffold that (per its
own doc comment) *hosts nothing yet*.

The defect is that `config::load` (`crates/agent-core/src/config.rs:2349`) is
not a neutral reader. On the both-copies-missing arm it logs
`tracing::error!(… "the host must be re-enrolled")`
(`crates/agent-core/src/config.rs:2379-2387`) before returning `Err`.

That severity is right for its *original* caller — the 2026-08-12 self-heal for
an all-NUL config, where the worker really was exit-1'ing every 60 s and
re-enrollment really was the remedy. It is wrong for a probe asking whether an
optional flag happens to be set somewhere it usually isn't.

**A shared helper logging at the severity of its worst caller.** Every caller
inherits the loudest interpretation of a failure, including the callers for whom
that failure is the normal case.

## Why it matters more than a noisy line

1. **It prescribes a destructive remedy.** Re-enrollment is not free: removal is
   final (`overlay_nodes` rows are tombstoned, `find_live_by_tenant_and_machine`
   is live-scoped), so a device that re-enrolls gets a **fresh lease and never
   its old address back**. An operator who believes this line renumbers a
   working host.
2. **It destroys the signal it exists to send.** The genuine all-NUL case emits
   the identical line, so the message that was designed to be unmissable is now
   indistinguishable from routine start-up noise. The self-heal's own comment
   says *"Loud, because silently running on an older config must never look like
   a normal boot"* — that property is already lost.
3. **It is invisible to every health check.** `roomler status`, `roomler peers`
   and the server's `is_online` all read healthy, so nothing contradicts the log.
   This is the inverse of the `systemctl is-active` trap in `CLAUDE.md`: there,
   a healthy host reads dead; here, a healthy host reads doomed.

## Design

**The probe must not go through the recovery path at all.** Absence of an
optional machine-global config is not a failure to be recovered from — it is an
expected input to a boolean question.

- `config::load` keeps its behaviour and its ERROR **unchanged**: its contract is
  *"load the config this process runs on"*, and for that caller the message and
  its severity are correct.
- Add a sibling for probes that returns `Option<AgentConfig>` and logs at most
  `debug!`, then point `netd_enabled()` at it. Naming it for the question rather
  than the mechanism (`load_optional` / `probe`) is what stops the next probe
  reaching for `load` again.
- ⚠️ Do **not** fix this by softening `load`'s ERROR to `warn!`. The all-NUL case
  is exactly as serious as it was, and quietening it to fix a caller that should
  not be calling it would trade a false alarm for a missed one.

## Phases

| phase | what | kill switch | status |
|---|---|---|---|
| P1 | non-logging probe reader + `netd_enabled()` uses it; unit-lock that the probe path emits no ERROR | revert (one function) | **planned** |
| P2 | audit every other `config::load` caller for the same shape — is the failure normal for that caller? | per-call-site | **planned** |
| P3 | field-verify on the host that produced the evidence: a service restart logs no ERROR, and an induced unreadable config still does | — | **planned** |

## Acceptance criteria

- [ ] A service start on a host with no machine-global config logs **no** ERROR.
- [ ] `netd_enabled()` still returns `false` in that case (behaviour unchanged).
- [ ] The env lever `ROOMLERD_OVERLAY_NETD` still wins over the file.
- [ ] An actually-unreadable **worker** config still logs `the host must be
      re-enrolled` at ERROR — shown failing before the fix and passing after, so
      the check is proven to still fire.
- [ ] Every remaining `config::load` caller is either correct at ERROR severity
      or moved to the probe reader, with the reason recorded per call site.
- [ ] Field-verified on the originating host across a real service restart.

## Open decisions

- Whether P2 finds a third caller worth moving. If it finds none, the probe
  reader is still justified — the point is that the next one has somewhere right
  to go.
- Whether the machine-global probe should exist at all on a user-context install,
  or whether `netd_enabled()` should read the worker's own resolved config. That
  is a bigger question about which config owns a supervisor-level flag, and it is
  deliberately **not** bundled here.

## Out of scope

- The ~2-hourly service restarts observed on the same host (01:47, 03:47, 05:48,
  06:06 on 2026-09-02, one worker exiting `code=2`). Same log, unrelated cause,
  and folding them together would make both harder to reason about. Worth its own
  FR once the cadence is characterised.
- The `C:\ProgramData\roomler-agent\peer-connected.lock` marker path in the same
  log block. That is a correctly-frozen FR-46 anchor, not a defect.

## Field-verification log

| date | build | what was proven |
|---|---|---|
| 2026-09-02 | 0.4.48, neo16 | The ERROR fires on every service start of a fully healthy host. Established that the worker runs `session_id=1 elevated=true` and uses `%APPDATA%\roomler\roomler\config\config.toml` (live, rewritten same day, healthy `.prev`), so the machine-global path it names is legitimately absent rather than lost. Traced to `netd_enabled()` probing that path for one optional flag through `config::load`, which logs the re-enroll ERROR on the both-copies-missing arm |
