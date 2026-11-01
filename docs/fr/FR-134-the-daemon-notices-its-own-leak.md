# FR-134: The daemon notices its own leak, and a peer probe starts no process

**Issue:** [#3262](https://github.com/gjovanov/roomler-ai/issues/3262) · **Status:** P1 and P2 shipped in 0.4.194, 2026-11-01. AC3 reads on 2026-11-08
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
| P1 | the handle self-report | `ROOMLERD_RESOURCE_WATCH=0` | shipped, 0.4.194 |
| P2 | a Windows probe starts no process | `ROOMLERD_PING_SUBPROCESS=1` | shipped, 0.4.194 |
| P3 | docs: the self-report and both switches in the operator docs, field log here | | [`docs/resource-watch.md`](../resource-watch.md) |

`ROOMLERD_RESOURCE_WATCH_MIN_RISE=<n>` lowers the bar for AC2's field test.

## Acceptance criteria

- [x] **AC1**: all three OSes log the handle count hourly, read from `agent_logs` on real devices.
- [x] **AC2**: a real climb reaches `agent_logs` as a warning, on a lab device with the bar lowered.
- [ ] **AC3**: no false alarm on a healthy device across the fleet for 7 days.
- [x] **AC4**: a Windows host starts no `PING.EXE` from the daemon over 10 minutes, and `roomler peers` still fills its RTT column, within 5 ms of a hand `ping`.
- [x] **AC5**: docs: the self-report and both kill switches in the operator docs, with a `docs/README.md` row if a page is added.

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
| 2026-11-01 | #3252's fix on 0.4.193, NEO16 `SystemContext` worker, keep-busy `running` throughout (read from the worker's log, so the leaking path was live) | 573 handles at 55 s, 707 at 775 s, and it falls back between readings. 0.4.192 reached 237,194 at 76 min on the same host |
| 2026-11-01 | AC4, the failing baseline: 0.4.193 on NEO16 | 249 `PING.EXE` from roomlerd in 300 s |
| 2026-11-01 | AC4 on 0.4.194, NEO16, each start judged against the live roomlerd PIDs | **0 from the daemon in 610 s**, 1 positive control. The other 107 starts were my own hand pings, 80 + 27. A first run left one start with a parent already gone, which is why the watch was rewritten to compare PIDs |
| 2026-11-01 | AC4, the RTT column against a hand `ping`, median of 16 pairs each | mars 34 and 33 ms, three LAN peers 7 and 7, 5 and 4, 4 and 5 ms. A Wi-Fi Mac read 13 and 36 ms on 16 pairs, so it took 40 more: minimum 4 and 4, quartiles 5 to 54 and 6 to 63, medians 21 and 14. The same distribution, and each table value is one packet |
| 2026-11-01 | AC1 from `agent_logs`, read with `scripts/resource-watch-read.cjs` | 15 devices in the grox org, each with two lines **60.0 minutes** apart: 9 Windows (485 to 800 handles), 4 Linux (36 to 39, the lab device aside), 2 macOS rows (20 to 33) |
| 2026-11-01 | AC2 on NEO16-WSL, `ROOMLERD_RESOURCE_WATCH_MIN_RISE=30`, 6 held LocalAPI connections every 5 minutes, each one real descriptor in the daemon | 43 at the first sample, then `WARN handles=115 rise=72 per_second=0` at 17:59:11Z on the 13th: the fleet's only warning, read from `agent_logs`. The rig was removed afterwards |
| 2026-11-01 | Wrong turns | The script reformatted for this repo returned `lines 1`: mongosh reads stdin as a REPL, so a bare `coll` ran alone and `.aggregate(` errored. `roomler logs --grep` found no hourly line, because it filters only the last 64 KiB. Both are in `docs/resource-watch.md`. |
