# FR-18: Carrier queue discipline

Status: **in progress** (2026-08-27; updated 2026-10-31). Tracking issue: `FR-18` (#801).
Child of FR-1; the lever that matters most for corp-VPN hosts. Shipped in 0.4.10 (#802)
as a partial pass. On 2026-10-30 the `/derp` send-buffer cap gained a read-back (#3151,
released in 0.4.189) and the docs AC was met. On 2026-10-31 the fleet read met AC4's field
half. The drag ACs wait on the CORPLAP-1 ↔ CORPLAP-3 relay pair.

## Why this is the priority — the LAN-relay test came back negative

Before committing to queue work, we tested whether a LAN-local DERP relay could serve
the two problem hosts, since all six machines share one desk and one Wi-Fi
(`192.168.68.0/24`). Method: a controlled TCP listener on neo16 (`192.168.68.126:47443`)
with an explicit inbound firewall rule, verified reachable from neo16 itself on both
loopback and its LAN address, then probed from each VPN host over Fleet RPC.

| host | VPN client | LAN route | result |
|---|---|---|---|
| CORPLAP-3 | Cisco AnyConnect | `192.168.68.0/24 → 10.138.80.1` (VPN gw) **metric 1**, beating the on-link WLAN route at metric 256 | unreachable — every LAN target, including the router |
| CORPLAP-1 | Check Point Endpoint | clean: only the on-link WLAN route | reachable on paper, **times out in practice** |

The listener recorded only neo16's own two self-tests; neither VPN host ever arrived.
neo16 holds live ARP entries for both machines (`192.168.68.106`, `.119`), so L2 is fine
and the AP is not isolating clients — the block is inside each VPN client. AnyConnect
enforces "no local LAN access" by hijacking the LAN prefix in the routing table; Check
Point enforces it in its endpoint firewall with the routes left intact.

Both are corporate policy on managed devices. **Working around them would be VPN policy
evasion and is out of scope.** The consequence is the load-bearing one: for these hosts
the DERP relay is **structural, not a fallback** — so the carrier's queue discipline is
not one lever among several, it is the whole game.

## Problem

A video frame crosses three stacked buffers on its way out, and the first two are ours:

| buffer | depth | at 3 Mbps |
|---|---|---|
| DERP outbound queue (`transport/derp.rs`, `OUTBOUND_QUEUE = 512`) | 512 frames ≈ 660 KB | ≈ **1.8 s** |
| kernel TCP send buffer on the DERP WS (no `SO_SNDBUF` cap) | autotuned, often MBs | 0.5–2 s |
| SCTP reliable+ordered queue above (FR-17 #799) | unbounded | seconds under loss |

Measured consequence: `send_wait_max_ms` of **10 263 ms** on CORPLAP-1 (hevc_qsv), 4 740
(vp9_qsv), 1 870 (h264_qsv), 907 on CORPLAP-3 (av1_qsv) — the time ONE frame spent inside the
DataChannel send call, while encode ran 8–12 ms and our own `bytes_inflight` held tens
of KB. The agent is healthy; it cannot hand bytes to the wire.

Two specific defects, both ours:

1. **The queue drops the wrong end.** `send_to` uses `try_send`, which discards the
   **newest** frame on overflow. For a loss-tolerant real-time carrier that is
   backwards: under sustained overload we retain 512 stale frames and throw away the
   fresh ones, so the receiver is served old data and the queue never refreshes.
2. **The queue is sized in frames, so its latency contribution scales with the
   reciprocal of the rate.** 512 frames is 33 ms at 160 Mbps and 1.8 s at 3 Mbps — and
   3 Mbps is exactly where these hosts live.

A datagram carrier that buffers instead of dropping converts loss into latency. That is
the bufferbloat pathology, one layer below where we have been looking.

## Design

**A. Deadline-aware dropping.** Stamp each frame on enqueue; the WS writer discards any
frame older than `derp_queue_max_age_ms` (default 100 ms) instead of sending it. This
bounds our layer's latency contribution **in time**, independent of rate, and it is
self-correcting: under overload the writer discards the stale head quickly and reaches
fresh frames, which is drop-oldest semantics without a custom queue structure.

**B. Cap the kernel send buffer** on the DERP TCP socket (`socket2::SockRef`, the
pattern already used in `driver.rs`). Without it, a frame the app just decided to send
can still sit for seconds in a kernel buffer the age check cannot reach — the app-level
discipline would simply drain into a deeper queue below it.

**C. Count both drop causes** (`dropped_stale`, `dropped_full`) and surface them, so the
next reader can tell "the carrier shed load" from "the carrier stalled".

Kill switch `derp_queue_max_age_ms = 0` restores the pre-FR-18 behaviour.

⚠️ Deliberately NOT drop-oldest via a custom queue: the age check achieves the same
effect at the consumer with no new data structure and no change to the mux's ownership
model. Revisit only if measurement shows the discard loop itself is a cost.

⚠️ This does not fix the base RTT (86–210 ms for a same-desk pair, because the relay is
on the internet). It fixes the queue *on top of* that RTT, which is where the seconds
are.

## Acceptance criteria

- [ ] On the CORPLAP-1 ↔ CORPLAP-3 relay pair under drag: `send_wait` p99 < 250 ms
      (was: 10 263 ms max) and no `send_wait` sample above 1 s.
- [ ] `dropped_stale` becomes non-zero under load while delivered fps does NOT fall —
      shedding stale frames must replace waiting, not replace delivery.
- [ ] Direct transports unchanged (a direct pair's `dropped_stale` stays ~0, since the
      queue never fills).
- [x] The kernel send buffer cap is verified applied (`getsockopt`), not merely
      requested. **Code half met (2026-10-30, #3151, released in 0.4.189):** after the set,
      `cap_send_buffer` reads the size back with `getsockopt(SO_SNDBUF)`, judges it
      in the request's units (Linux reports twice the size in force) and logs one
      line per `/derp` connect with `requested`, `applied` and `clamped`
      (`agents/roomlerd/src/derp.rs:71`, `:135`, `:165`). Unit tests cover the
      verdict per OS, a real loopback socket through a WebSocket, a filled socket
      keeping its cap, and the line's fields
      (`agents/roomlerd/src/derp.rs:675`, `:753`, `:781`, `:841`). They
      are green on Windows and on Linux 6.6. **Field half met (2026-10-31, on
      0.4.191):** a read-only log read shows the *"overlay derp: send buffer"* line
      with the size each kernel applied: `clamped=true applied=212992` on mars, zeus
      and jupiter (Linux 6.8, `net.core.wmem_max` 212992), and `applied=262144
      clamped=false` on MacBook-1, on two Windows hosts through `roomler exec`, and on
      CORPLAP-1, CORPLAP-2 and CORPLAP-3 through the server's `agent_logs`. Across
      the org, none of the 12 devices that ran since 0.4.189 logged a refused,
      exceeded or unread cap (field log, 2026-10-31).
- [ ] Field: the FR-1 age pill on a relayed pair no longer shows multi-second
      excursions.
- [x] Docs: the carrier's queue discipline (the age bound, the send-buffer cap
      and its per-OS read-back, the counter, and what each log line means) is in
      `docs/overlay-communication.md` §6, with a diagram, and indexed in the
      `docs/README.md` row for that doc (2026-10-30).

## Field log

| date | build | result |
|---|---|---|
| 2026-08-27 | 0.4.9 | LAN-relay test negative (above); baseline `send_wait` numbers recorded; FR filed. |
| 2026-08-28 | 0.4.10 | **Partial pass.** Operator: "it works much much smoother now." Same host, same pair, split by emitting version: CORPLAP-3 `send_wait` **p99 2 995 → 200 ms**, max 7 826 → 1 778 ms, p50 unchanged (0.158 → 0.164 — the gate costs nothing at steady state), viewer age max 4 087 → 2 185 ms, skips 2 → 58 (the intended trade). CORPLAP-1 on 0.4.10: `send_wait` max 1.66 ms. **Not closed**: max is still 1 778 ms (target: no sample > 1 s) and age excursions are still multi-second; residual belongs to the layers this FR does not touch (SCTP HOL, FR-17 #799) plus the FR-15 floor bug, observed live here as `viewer_age_floor_ms = 2` on a 44 ms-per-leg path. ⚠️ `dropped_stale` could NOT be evaluated — the counter was added without a reader (`stale_drops()` has no consumers); that instrumentation gap must close before the shed-vs-deliver criterion means anything. |
| 2026-10-30 | master `3fdccc44f` + the AC4 read-back PR (unreleased) | **AC4, code half.** The daemon now reads the `/derp` socket's send buffer back after capping it, instead of trusting the set. **RED first:** in a Linux 6.6 container at stock sysctls, a 262144 request returns `Ok` and `getsockopt(SO_SNDBUF)` then reports 425984, which is **212992 in force**. `net.core.wmem_max` defaults to 212992 before Linux 6.18 (kernel commit `a6d4f25888b8` raised it to 4 MiB), so the old code assumed 256 KiB on a socket holding 208 KiB. The same kernel reads a 65536 request back as 131072, the doubling. Each OS's rule is taken from its docs or source: socket(7) and `net/core/sock.c`; the Winsock `getsockopt` and `SIO_IDEAL_SEND_BACKLOG_QUERY` pages; XNU's `uipc_socket.c`, `uipc_socket2.c` and `tcp_output.c`. On all three, the set turns the socket's send-buffer autotuning off, so one read after the set holds for the connection. **GREEN:** the cap logs one line per connect with `requested`, `applied` and `clamped` (`agents/roomlerd/src/derp.rs:165`). Through the real code path, a loopback socket reads back `Clamped(212992)` on Linux 6.6 and `Applied(262144)` on Windows, and the 5 new `derp::tests` are green on both. Docs: `docs/overlay-communication.md` §6. **Still open:** the field read, after a release. Found on the way: the 08-28 reader gap is closed on master, where the keepalive tick logs `dropped_stale` (`agents/roomlerd/src/derp.rs:440`). Design C's `dropped_full` was never built: a full outbound queue drops a frame uncounted (`crates/tunnel-core/src/transport/derp.rs:323`). |
| 2026-10-31 | 0.4.191 on every host read; lines from 0.4.189 on | **AC4, field half: met.** Read-only, nothing restarted. Through `roomler exec` (`roomler logs --grep 'overlay derp: send buffer'`), mars, zeus and jupiter (Linux 6.8, `net.core.wmem_max` 212992) log `capped below the request` with `requested=262144 applied=212992 clamped=true`, on the central socket and on a regional relay's. MacBook-1 (macOS 27.0.1, `kern.ipc.maxsockbuf` 8 MiB) and two Windows 11 hosts log `capped — the kernel holds the requested size` with `applied=262144 clamped=false`. CORPLAP-1, CORPLAP-2 and CORPLAP-3 take no exec, so theirs came from the server's `agent_logs`: 35, 21 and 14 connects, all `applied=262144 clamped=false`, from 0.4.189 to 0.4.191. **Control:** zeus's newest line in `agent_logs` and zeus's own log are one line to the millisecond (21:09:59.035Z, `applied=212992`), so the server route reads what the host wrote. **Before the release:** the line did not exist. On mars, zeus and jupiter together, `agent_logs` holds 204 `overlay derp:` lines from 0.4.186 to 0.4.188 and no send-buffer line among them; those start at 0.4.189. **Census of the org, not a sample:** `agent_logs` since 0.4.189's publish (2026-10-30 20:41Z), read at 21:28Z the next day: 56 556 batches, 193 lines from 12 devices. Those are all 12 devices of the server's 20 rows that ran in that window. 66 lines read 212992 on the 4 Linux devices and 127 read 262144 on the other 8, every one INFO. Nobody logged `not capped`, `cap refused`, `not read back` or `left at the OS default`, and that zero can be trusted: WARN-level `overlay derp:` lines from those same Linux hosts do reach the collection. So the cap is real on all three OSes. On a stock Linux kernel it sits 19 % under the request, at 208 KiB, still well above the 75 KB bandwidth-delay product `docs/overlay-communication.md` §6 sized it for. |
