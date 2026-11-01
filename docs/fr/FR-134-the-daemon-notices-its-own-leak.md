# FR-134: The daemon notices its own leak, and a peer probe starts no process

**Issue:** [#3262](https://github.com/gjovanov/roomler-ai/issues/3262) · **Status:** P1 and P2 in review 2026-11-01
**Related:** #3252 (the leak that prompted this), FR-65 ([#1255](https://github.com/gjovanov/roomler-ai/issues/1255), the stall watch), FR-92 (keep-busy, the leak's constant caller)

## Goal

**A leak in the daemon's own handles shows up in the fleet's logs within the hour, not when the
daemon stops working.** And a peer probe stops starting a process.

## Evidence

Anchors are master `8a0e297fe`, the bump to 0.4.193.

### Nothing reported a six-month leak

`try_change_desktop` bound the thread to a fresh handle and dropped it. `CloseDesktop` fails with
`ERROR_BUSY` on a bound handle, and the failure was ignored, so every call leaked one. That has
been true since the rebind became unconditional on 2026-05-06. #3252 fixed it on 2026-11-01.

| measured on a `SystemContext` worker with keep-busy on | |
|---|---|
| 0.4.191 | 51 K handles at 26 min |
| 0.4.192 | 79,634 at 25 min, 180,227 at 51 min |
| rate | 59 a second, keep-busy's 33 ms tick at about two jobs a tick |
| Windows' per-process limit | 16.7 M, about 3.2 days at that rate |

Past the limit, everything in the daemon that opens a handle fails: sockets, files, threads. A
self-update restarted each worker first, so no device ever got that far and no log ever said so. It
was found by accident, while tracing a kernel leak on the host.

### The prober starts a process per peer

`OsPinger` shells `ping` once per probe ([`localapi_state.rs:61`](../../agents/roomlerd/src/localapi_state.rs#L61)),
every [`RTT_PROBE_INTERVAL`](../../agents/roomlerd/src/localapi_state.rs#L28) for each live peer. A
kernel trace on a host with 17 peers counted 26 `PING.EXE` starts in 23 s and 194 in 291 s, each
with its own `conhost.exe`. So the daemon alone was one to two process exits a second at idle.

That is more than wasted CPU where the host's display driver leaks by the process exit, as AMD's
`amdkmdag.sys` did on the measured host in both versions tried, 32.0.13034 and 32.0.21045.

## Design

### P1: the daemon samples its own handles

[`resource_watch.rs`](../../agents/roomlerd/src/resource_watch.rs) reads the count every 5 minutes:
`GetProcessHandleCount` on Windows, `/dev/fd` on Linux and macOS. It logs the count hourly, and
warns hourly while the count climbs. Every event reaches `agent_logs`
(`crates/agent-core/src/logs_upload.rs`), so a fleet read needs no new wire.

⚠️ **It only reports.** A daemon that leaks may still be the only way into the machine, and
exiting one nothing supervises takes the device offline for good. The same rule binds remote
configuration (`.claude/rules/networking.md`).

A climb is the lowest of the newest 3 samples sitting at least 2,000 handles above the highest of
the oldest 3, an hour apart (200 on Linux and macOS, where a process can have as few as 1,024). This
reads the two ends of the window rather than asking for a rise at every step, because a real leak
pauses (keep-busy stops while the person is at the machine) and other handles come and go around
it. A spike that fell back fails the test, and a one-off step clears once it leaves the window.

### P2: a probe calls the API `ping.exe` calls

On Windows [`icmp_win.rs`](../../agents/roomlerd/src/icmp_win.rs) calls `IcmpSendEcho` and
`Icmp6SendEcho2` off the runtime. The route is the same as `ping`'s, the number is the OS's own, and
nothing starts. Linux and macOS keep shelling `ping`: nothing measured a cost there, and a raw socket
would need privileges a per-user install lacks.

## Phases

| phase | what | kill switch | status |
|---|---|---|---|
| P1 | the handle self-report | `ROOMLERD_RESOURCE_WATCH=0` | in review |
| P2 | a Windows probe starts no process | `ROOMLERD_PING_SUBPROCESS=1` | in review |
| P3 | docs: the self-report and both switches in the operator docs, field log here | | owed before close |

`ROOMLERD_RESOURCE_WATCH_MIN_RISE=<n>` lowers the bar for AC2's field test.

## Acceptance criteria

- [ ] **AC1**: all three OSes log the handle count hourly, read from `agent_logs` on real devices.
- [ ] **AC2**: a real climb reaches `agent_logs` as a warning, on a lab device with the bar lowered.
- [ ] **AC3**: no false alarm on a healthy device across the fleet for 7 days.
- [ ] **AC4**: a Windows host starts no `PING.EXE` from the daemon over 10 minutes, and `roomler peers` still fills its RTT column, within 5 ms of a hand `ping`.
- [ ] **AC5**: docs: the self-report and both kill switches in the operator docs, with a `docs/README.md` row if a page is added.

## Open decisions

None blocking. AC3 is what would change the thresholds.

## Out of scope

- Restarting a daemon that leaks (see P1's warning).
- Showing the count in the web UI.
- ICMP without `ping` on Linux and macOS.
- The kernel leaks in AMD's display driver, which are the vendor's to fix.

## Field log

| date | what | result |
|---|---|---|
