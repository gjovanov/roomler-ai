# FR-132: Zero-copy by default — each GPU-input cell turns on for the fleet after a clean week of its own

**Issue:** [#3141](https://github.com/gjovanov/roomler-ai/issues/3141) · **Status:** P1 released in 0.4.191 on 2026-10-31;
P1b on production since 2026-11-03 and P1c released in 0.4.199 the same day (§4); the decisions taken 2026-10-30, 2026-10-31 and 2026-11-02 (§6); proposed 2026-10-30 · **Owner:** the agent's capture and encode path, and
remote config · **Anchors:** master `01520c8af` (§2); the as-built notes in §3a and §3b, master `ff29120ed` + P1 ·
**Takes over:** [FR-118](FR-118-gpu-resident-frames.md) open decision 2 (the default) · **Builds on:**
[FR-118](FR-118-gpu-resident-frames.md) (the GPU-input cells), [FR-77](FR-77-encoder-chroma-matrix.md) (cells, the
probe, `encoder_cells_deny`), [FR-119](FR-119-high-refresh-streaming.md) (`fps_deny`, the second pushable denylist)

## 1. Goal

On a host where FR-118's GPU path is proven, a session takes it without anyone setting a switch on that host. The
default goes on **one cell at a time**, and a cell goes on only after a clean fleet week of its own. Before any cell
goes on, the server can turn any cell off on any device.

FR-118 closed with every cell it built field-verified, and with `zero_copy` **off by default**: the decision it left
open (§7, decision 2) was "OFF for one release, then ON per cell after a clean fleet week of that cell (decode errors,
keyframe requests, `input_path` reasons), never globally". The first half has held since 0.4.181. This FR is the
second half, and §2 shows that two things it needs are missing today.

## 2. Evidence (master `01520c8af`)

### 2.1 What the GPU path saves, per cell

| Cell | Host | CPU path → GPU path | FR-118 |
|---|---|---|---|
| QSV, the DXGI arm | CORPLAP-3, `av1_qsv` 1920×1200 | capture + encode 14.70 → 12.30 ms a frame; the worker's CPU 15.9 → 10.6 ms an encoded frame | §9.3 |
| QSV, the DXGI arm | the 2880×1800 laptop, `av1_qsv` | windows at ≥ 50 fps 0 of 564 → 594 of 596; capture + encode 25.12 → 16.61 ms; CPU 15.93 → 3.88 ms | §9.10 |
| NVENC, the WGC arm | neo16, `av1_nvenc` 2560×1600 | readback + encode 18.26 → 5.14 ms; CPU 9.64 → 3.18 ms | §9.7 |
| VideoToolbox | the MacBook, `hevc_videotoolbox` 3024×1964 | encode stage 4.18 → 0.12 ms; CPU 14.58 → 2.55 ms | §9.9 |

Every one of those legs ran with the switch set by hand on a test host. No other host runs the GPU path.

### 2.2 The switch is off unless a host sets it

`zero_copy_enabled` reads `ROOMLERD_ZERO_COPY` (config `zero_copy`), and any value other than `1`, `true`, `yes` or
`on` is off, unset included (`agents/roomlerd/src/capture/mod.rs:91-107`). A capture backend reads it once when it
opens. An encoder then opens the GPU form only for a cell the probe's `gpu` child proved
(`agents/roomlerd/src/capture/gpu_frame.rs:52-59`) and `zero_copy_deny` does not name
(`agents/roomlerd/src/encode/ffmpeg/gpu_hint.rs:142-169`), and otherwise reports CPU frames and the reason
(`gpu_hint.rs:85-97`). The config surface offers both keys locally, restart-applied
(`crates/agent-core/src/config_surface.rs:1755-1778`).

### 2.3 The server cannot turn a cell off

`DesiredConfig`, what remote config pushes, carries two keys of this family, `encoder_cells_deny` and `fps_deny`
(`crates/remote_control/src/models.rs:2976-2990`), and the agent's `apply` handles exactly those two
(`agents/roomlerd/src/remote_config.rs:308-332`). Neither `zero_copy` nor `zero_copy_deny` is there. Today that
costs nothing, because the switch is off everywhere. With a default on, a cell that misbehaves on one driver could
be turned off only one host at a time, over exec, and only on hosts that answer exec.

### 2.4 A fleet week cannot count decode errors

A decode error in the viewer's WebCodecs worker is posted to the page (`ui/src/workers/rc-webcodecs-worker.ts:170-182`),
which tears the WebCodecs path down and falls back to `<video>` (`ui/src/composables/useRemoteControl.ts:7752-7765`).
Nothing reaches the agent or the server. What does reach the server per session is `rc:session.stats`, whose
`keyframe_requests` counts the viewer's keyframe asks (`agents/roomlerd/src/peer.rs:334-350`). FR-118's legs read
decode errors in a viewer someone was watching, and a fleet week has no such viewer. So the first measure decision
2 names cannot be read today.

## 3. Design

```mermaid
flowchart TD
  A[an encoder opens a cell] --> B{zero_copy}
  B -->|off| C[CPU frames: switch_off]
  B -->|on| E{proven, not denied,<br/>same adapter?}
  B -->|unset| D{cell in the<br/>default list?}
  D -->|no| F[CPU frames: default_off]
  D -->|yes| E
  E -->|no| G[CPU frames: unproven / denied /<br/>adapter_mismatch]
  E -->|yes| H[GPU frames]
```

### 3a. The kill switch first (P1)

`zero_copy_deny` joins `DesiredConfig`, beside `encoder_cells_deny` and `fps_deny`, and for their reason: it only
ever removes cells a device would otherwise open, so it is not a security gate and `MANAGE_AGENTS` is enough. The
agent applies it as it applies `fps_deny` (a blank push clears it, `none` denies nothing) and reports
`needs_restart`, since the probe and the config fallbacks read it once per process. It ships before any cell is on
by default, so the first flip already has a switch the server can push.

As built: the route passes the text on unread, as it does the other two denylists, because an entry the gate cannot
read denies nothing (`crates/modules/fleet/src/remote_config.rs:206`). The agent stores it trimmed, a blank clears
it, and it reports `needs_restart` (`agents/roomlerd/src/remote_config.rs:354`). A test chains the pushed value
through the restarted daemon's config fallbacks to the exact `name:gpu` entry `gpu_hint::is_denied` matches.
Decision 1 is held by `the_device_owned_refusals_are_not_pushable`, which compares whole keys
(`crates/remote_control/src/models.rs:9540`). The dialog gets the field, and
[`remote-config.md`](../remote-config.md) §6 the key, with its value table.

**P1b, the push reaches a connected device.** AC2's field run (§8) showed what the push cost a kill switch: reconcile
on connect was the only delivery path, so a device that stayed connected never heard it, and the deny sat undelivered
for 71 minutes. A save now nudges a connected device with the same frame, from the pod that holds its socket or over
the rc ctrl lane (decision 5). That is the nudge [`remote-config.md`](../remote-config.md) §6 always described.
Reconcile on connect stays what a device converges by, so a lost nudge costs only the wait for the next connect, and
nothing about a save restarts a daemon. The device has no revision check of its own, so one hub call sends every
`rc:agent.config` and never gives a connection a revision at or below one it carried.

**P1c, `needs_restart` stays true.** Reading P1b's code found a false `applied`. A report is last-write-wins, and
`apply` compares the desired value with the FILE, so once a value is written, any later frame on any reconnect answers
`noop` and the row reads `applied`, though the daemon still runs the old value. A pod roll does that to a whole fleet,
and a kill switch that says it is in force when it is not is worse than a slow one. The server cannot tell a reconnect
from a restart, since no process identity crosses the wire, so this is the agent's to fix: a key stays in
`needs_restart` until the running process holds the value. As built, the list is exactly what the process does not
run, so a save back to the running value leaves nothing waiting and reads `applied` at once. An independent read of
this change also found the live `exec_enabled` gate moving only when the file changed, so a push could read `applied`
over a gate that had not moved. It now follows the document, and a gate that moves is always in the report's `live`.

### 3b. Decode errors become countable (P1)

The viewer sends one control message per decode error, with the stage that failed (configure, decode, the pipe),
before it falls back. The agent counts them per session, as it counts `keyframe_requests`, and `rc:session.stats`
carries the count. A viewer without this sends nothing, which reads as zero, so the canary read in 3d compares
sessions from viewers that have it.

As built, a session's count tells "zero" from "not counted":

- Every `rc:decodestat` carries `de: 1`, so the agent knows this viewer counts. `rc:session.stats` then carries
  `decode_errors`, and `Some(0)` is a measured zero. A session whose viewer or agent predates P1, or that never
  decoded in a worker, carries no field, and the server writes none. The canary read compares only rows that have it.
- `rc:decode-error` carries `stage` (`configure` · `decode` · `pipe`), `path` (`webcodecs` · `vp9-444` · `hevc`),
  and the codec and the decoder's message, cut short.
- The agent logs the first eight errors with their stage.
- The first `de`, and each of those first eight errors, send one sample at once, by the carrier's road
  (FR-102 P9d). A session someone leaves right after a black screen still records its count, and a short clean
  session records its zero.
- A re-offer carries the count to the new peer.
- The server writes `stats.decode_errors` only from a sample that counted.

Detail: [remote-control.md §11.10](../remote-control.md#1110-the-viewers-decode-errors-fr-132-p1).

### 3c. A default per cell (P2)

`zero_copy` becomes three-valued in its reading as it already is in config (`kind: "tribool"`): `on` keeps today's
meaning (every proven cell), `off` turns every cell off, and **unset** means "the cells in the built-in default list".
A cell outside the list reports CPU frames and a new reason, `default_off`, so "not yet on by default" is never read
as "someone turned it off" (`switch_off`). The probe's proof, the adapter rule and `zero_copy_deny` apply to a
listed cell exactly as to one turned on by hand.

The capture decides at open, before the encoder's cell is known. With the switch unset it delivers GPU frames when
the first proven GPU-input cell in cascade order is listed, the cell whose vendor `gpu_frame.rs` already uses to pick
WGC's adapter. An encoder on an unlisted cell then reads those frames back through `Frame::cpu()`, which returns
today's readback bytes (FR-118 AC2). That costs what the CPU path costs already.

The list ships **empty**, so P2 alone changes nothing a session does. A test pins that: with the list empty and
`zero_copy` unset, every input form matches `off`'s, and only the reason differs.

### 3d. A canary week per cell, then its flip (P3–P5)

For each cell, its canary hosts (open decision 2) set `zero_copy = on` locally and run for 7 days. That week is read
against the same hosts' CPU-path week before it, from the heartbeats and `rc:session.stats`:

| Measure | Where | Clean means |
|---|---|---|
| decode errors per session-hour | `rc:session.stats` (3b) | no rise (open decision 3) |
| keyframe requests per session-hour | `rc:session.stats` | no rise |
| `input_reason` | the DC heartbeat (`agents/roomlerd/src/peer.rs:10019-10033`) | `gpu` on every session of the cell; no `open_failed` that persists |
| capture + encode, CPU per frame | the heartbeat; the process's CPU time | FR-118's saving for that cell, roughly |

The capture + encode row reads moving and idle windows apart, with under 5 fps captured as idle. FR-118 measured its
savings under motion, and an idle window spends most of its encodes on keepalives, which read nothing back, so a mostly
idle week can show neither a saving nor a harm (§8, 2026-11-02). It also keeps to builds that report `input_path`,
because an older build's `avg_readback_ms` reads 0 whatever the capture cost.

A clean week puts the cell on the default list in the next agent release. A fleet week after that release verifies
the flip on every host that runs the cell, canary or not.

## 4. Phases

| P | Scope | Kill switch | Status |
|---|---|---|---|
| P0 | The red record: a fleet week of heartbeats read per cell (encoder × capture arm), counting the sessions that could have opened a GPU form and read `switch_off`, and naming the hosts that run each cell | — | **done 2026-10-30** (§8, AC1) |
| P1 | 3a and 3b: `zero_copy_deny` in `DesiredConfig` and the agent's `apply`; the viewer's decode-error message and its count in `rc:session.stats` | the push is absent (no change); a viewer without the message sends nothing | **built 2026-10-30, released 2026-10-31** ([#3154](https://github.com/gjovanov/roomler-ai/pull/3154), in 0.4.191 and `hosted-20261031-28dfd72`). 3a: `MANAGE_AGENTS` alone, passed on unread, written by the device with `needs_restart`, end to end in `agent_tests` (AC2's integration half). 3b: `rc:decode-error` per error before the fallback, `de: 1` on every `rc:decodestat`, `decode_errors` on `rc:session.stats` (absent = not counted), the first `de` and the first eight errors sent at once, end to end through a real agent and viewer peer in `session_decode_errors_tests` (AC3's integration half). The field halves of AC2 and AC3 are met (§8) |
| P1b | 3a: a save nudges a connected device with the same frame, and one hub call sends every `rc:agent.config` | the previous image: the connect path never left, so a device still converges | **built** ([#3242](https://github.com/gjovanov/roomler-ai/pull/3242)): server and UI only, so it needs a promote and no release. **On production since 2026-11-03** (`hosted-20261103-5f12695`); AC8's field half in §8 |
| P1c | 3a: `needs_restart` holds until the daemon runs the value | the previous agent: a reconnect reads `applied` again | **built** ([#3283](https://github.com/gjovanov/roomler-ai/pull/3283)) and **released in 0.4.199** (2026-11-03); AC9's field half is read on it |
| P1d | 3d: `rc:session.stats` carries the encoder, the capture arm and the input form (decision 8) | absent from an older agent, and the week reads the heartbeats | not started |
| P2 | 3c: the three-valued reading, the default list (empty), `default_off`, at both entry points | `zero_copy = off`; the list empty | not started |
| P3 | QSV: the canary week, then the flip (if open decision 4 orders it first). `hevc_qsv` and `h264_qsv` also wait for decision 9 | `zero_copy_deny` pushed; the cell off the list in the next release | not started |
| P4 | NVENC on the WGC arm: the same | the same | not started |
| P5 | VideoToolbox: the same | the same | not started |
| P6 | Docs: the default per cell and each flip in `docs/encoders.md`, the pushable deny list in `docs/remote-config.md`, `docs/remote-control.md` §5.1f | — | not started |

## 5. Acceptance criteria

- [x] **AC1 (red first):** a fleet week of heartbeats counts the sessions that ran on a cell with a proven GPU form
  and read `input_reason=switch_off`, per cell, with the hosts that ran them.
- [x] **AC2 (the kill switch):** a `zero_copy_deny` push reaches a device, the device reports `needs_restart`, and
  after the restart that cell's sessions read `input_reason=denied`. Integration-tested, and seen in the field.
- [x] **AC3 (decode errors counted):** a decode error forced in the viewer shows up in that session's
  `rc:session.stats`, and a session with none reads zero.
- [ ] **AC4 (a default per cell):** with `zero_copy` unset, a listed cell opens GPU input and an unlisted one reads
  `default_off`; `on` and `off` keep their meaning; with the list empty, every input form matches `off`'s. Tested at
  both entry points, the probe and a session, as FR-118 AC6 was.
- [ ] **AC5 (a clean week before each flip):** per cell, 7 days with the switch on, read against the CPU-path week
  on the same hosts by the table in 3d, before the release that lists the cell.
- [ ] **AC6 (each flip, in the field):** after the release that lists a cell, a fleet week shows that cell's
  sessions on `input_path=gpu` on hosts that never set the switch, with no rise in decode errors or keyframe requests,
  and the per-frame saving FR-118 measured for the cell.
- [ ] **AC7 (docs):** `docs/encoders.md` (the default per cell and the record of each flip), `docs/remote-config.md`
  (the pushable deny list) and `docs/remote-control.md` §5.1f, with a row in `docs/README.md`'s index.
- [ ] **AC8 (the switch's reach):** a `zero_copy_deny` saved for a connected device reports `needs_restart` within
  seconds, with no reconnect, whichever pod takes the save; a device offline at the save converges on connect; and a
  connection never receives a revision after a newer one. Integration-tested, and seen in the field. **Field so far**
  (§8, 2026-11-03, `hosted-20261103-5f12695`): the connected device, the offline one and the order are seen; a save
  that lands on the pod without the device's socket is not, because the load balancer keeps a tenant's requests and
  sockets on one pod.
- [ ] **AC9 (`needs_restart` is true):** a reconnect without a restart leaves the row on `needs_restart`, and it reads
  `applied` only once the daemon runs the value. Tested, and seen in the field. **Red first** (§8, 2026-11-03): on
  0.4.197, the promote's reconnect turned a true `needs_restart` into `noop`.
- [ ] **AC10 (the week's source):** a session's `rc:session.stats` names its encoder, capture arm and input form on an
  agent that sends them, and they are absent on one that does not. Seen in the field.

## 6. Decisions

All four were decided by the operator on 2026-10-30, each as recommended
([#3141 comment](https://github.com/gjovanov/roomler-ai/issues/3141#issuecomment-6524105510)).

1. **Should `zero_copy` itself be pushable**, so a canary can be started from the server? **Decided: no.** Only
   the deny list is pushed, the half that removes, as with `encoder_cells_deny` and `fps_deny`. Turning a cell on
   stays a decision made on the device, or by a release. P1 holds it in a test that compares whole keys
   (`zero_copy` is a prefix of `zero_copy_deny`).
2. **Which hosts are each cell's canaries.** **Decided: picked from P0's list.** P0 names the hosts that run each
   cell. The 2880×1800 laptop is not a test host, so it joins a canary only in a window its owner agrees to, as in
   FR-118's decision 7.
3. **What "clean" means in numbers.** **Decided: within the spread of the CPU-path week on the same hosts**, with a
   floor of one event in the week, since a single host's week can be all zeros.
4. **The order of the flips.** **Decided: by the hosts each cell covers**, read from P0. QSV is likely first: every
   QSV host captures through the DXGI arm (FR-118 §9, 2026-10-27).

The next four were decided by the operator on 2026-10-31, each as recommended.

5. **Should a save reach a connected device, or wait for its next reconnect?** **Decided: reach it.** A kill switch
   that waits for a socket to drop is not much of a switch (§8). The server sends the same frame on a save, and
   restarts stay out of its hands ([`remote-config.md`](../remote-config.md) §7b).
6. **The canary hosts, by decision 2's rule.** **Decided:** QSV on CORPLAP-1 and CORPLAP-3. NVENC on NEO16, in a window
   its owner agrees to, and on CORPLAP-2 as a negative canary, whose SystemContext DXGI arm should read
   `adapter_mismatch`. VideoToolbox on MacBook-1. Each starts with a CPU week, because decode errors are counted only
   from 0.4.191.
7. **Should the probe prove a GPU form on a host with the switch off**, so the fleet knows which hosts hold a proven
   cell before any of them runs one? **Decided: no.** A canary's own week proves its cell, and P0's list is enough to
   pick the canaries.
8. **Where a canary week is read from.** **Decided: an extended `rc:session.stats`**, with the encoder, the capture arm
   and the input form on it. The heartbeat carries no capture arm, and a week read per session from `remote_sessions`
   does not depend on a grep of `agent_logs`.

The last came from an independent review of the RC media plan on 2026-11-02, with the numbers in §8. The operator took
it the same day, as recommended and open to being overturned, with [FR-135](FR-135-one-rc-media-pipeline.md)'s
decisions, where it is D16.

9. **Should the GPU form reach the office desktops, and how?** Decision 6 leaves out the class that runs `hevc_qsv`.
   Four office desktops ran 45.3 of its 47.6 session-hours in the week read. Its one canary, CORPLAP-1, ran 2.3, and
   CORPLAP-3 has no `hevc_qsv`. So a flip puts the first GPU-form frames on that class at the fleet flip, by
   auto-update, on PCs people are working on, and no read names their GPU or driver. A pushed `zero_copy_deny`
   probably does not reach them either: the remote-config opt-in is off by default and out of the server's reach, and
   none of the four has a config report on its row. So the rollback there is a forward release. The flip also buys
   them little: 43.88 of the cell's 46.97 windowed hours are idle, where a keystroke's frame would save about 2.7 ms of
   readback and a keepalive nothing. **Decided: hold.** `hevc_qsv` and `h264_qsv`, the two GPU-input cells those PCs
   advertise, stay off the default list until one of the PCs has run a canary week, opted in, in a window its owner
   agrees to. FR-135's D2 adds one more step for these two: after that canary, R5 or a read that the floor costs that
   class nothing. `av1_qsv`, NVENC and VideoToolbox wait for neither, since those PCs advertise none of them.

## 7. Out of scope

- New GPU-input cells: AMF and VAAPI (FR-118 deferred both with measurements), D3D12 and Vulkan, the portal and mutter
  arms (FR-118 §8).
- HDR, and the colour description, which is [FR-130](FR-130-colours-decode-true.md).
- Turning the default on for every cell at once. Decision 2 ruled that out.

## 8. Field-verification log

### 2026-10-30: P0, a fleet week of heartbeats (AC1)

Production `agent_logs` for the seven days before the read, grouped per day by host, encoder, `input_path` and
`input_reason`. 99 sessions read `switch_off`, the first gate, so the switch stopped them before any of the cell's
own checks.

| Family | Encoder | `switch_off` sessions | Hosts | Sessions on `input_path=gpu` |
|---|---|---|---|---|
| QSV | `hevc_qsv` | 36 | three office desktops (29), CORPLAP-1 (7) | 0 |
| QSV | `av1_qsv` | 13 | the 2880×1800 laptop (10), CORPLAP-3 (3) | 19 |
| QSV | `h264_qsv` | 0 | none | 1 |
| NVENC | `av1_nvenc` | 23 | CORPLAP-2 (21), NEO16 (2) | 2 |
| NVENC | `h264_nvenc` | 3 | NEO16 | 0 |
| NVENC | `hevc_nvenc` | 3 | NEO16 | 0 |
| VideoToolbox | `hevc_videotoolbox` | 20 | MacBook-1 | 2 |
| VideoToolbox | `h264_videotoolbox` | 1 | MacBook-1 | 0 |

The heartbeat carries no capture arm, so the arm comes from the capture-backend read beside it. Every QSV host
captures through the SystemContext DXGI arm. NVENC takes GPU frames only on the WGC arm, so CORPLAP-2's 21 sessions,
on that same DXGI arm, would likely read `adapter_mismatch` with the switch on. NVENC's count is an upper bound.

### 2026-10-31: AC3, decode errors counted, on 0.4.191

CORPLAP-3, `av1_qsv`, direct. The viewer was Chrome on NEO16, on `hosted-20261031-28dfd72`.

| Session | Device | What the viewer sent | `remote_sessions.stats.decode_errors` |
|---|---|---|---|
| clean, 24 s | 0.4.191 | `de: 1` on all 23 reports, no error | `0` |
| forced, 15 s | 0.4.191 | one `rc:decode-error` | `1` |
| the viewer's own reconnect, 16 s | 0.4.191 | one `rc:decode-error` | `1` |
| control, 21 s | 0.4.190 | `de: 1` on all 20 reports | absent |

The error was the decoder's own. The page wrapped its decode worker so that the worker's 100th `decode()` received a
2 KB random key chunk, and Chrome's `VideoDecoder` answered `Decoding error.`. The viewer sent `rc:decode-error`, and
the agent logged it with `stage=decode path=vp9-444 codec=av01.0.13M.08` and counted it. The wrapper was still armed
when the viewer reconnected, so the second session failed the same way and counted its own one.

The control is the state a promote leaves until the agents update: a 0.4.190 agent ignored `de`, the session ran
normally, and its row carries no count, which reads as not counted, not as zero.

Seen on the way: a mid-session decode error on this data-channel path froze the picture ("video stalled") for about
10 s, 9.4 s and 10.2 s from the error to the next session, and then a new session started by itself. So a real
failure reads in a fleet week as one row with a count and a clean row right after it, and the person at the viewer
sees a 10 s freeze.

### 2026-10-31: AC2's field half, pushed and open

The integration half is in `agent_tests` (P1). In the field the host is CORPLAP-3, which has the switch on and the
remote-config opt-in, and whose sessions read `encoder=av1_qsv input_path=gpu` before the push. The deny is
`h264_qsv:gpu`, not its everyday `av1_qsv`, so it costs the host's sessions nothing while it waits. It went through
the Device configuration dialog as revision 2 with `encoder_cells_deny` kept, and the device row reads `pending`.

A push lands on connect and takes effect at the restart after it
([`remote-config.md`](../remote-config.md) §6), and a restart can only be forced remotely with `roomler exec`, which
a corporate laptop does not take. Owed, in order:

1. The device's next reconnect, from a pod roll or the laptop waking: the row reads `needs_restart`.
2. The restart after that, from the next release's push: an `h264_qsv` session reads `input_reason=denied`, and AC2
   is met.
3. `none` pushed in place of the deny, so the cell is free again from the restart that follows.

### 2026-10-31: AC2 met on CORPLAP-3, and the cell freed again

This did not need the release the entry above expected. `roomler restart` at the device is the verb behind the
companion's Apply now, and it takes no `roomler exec` and no elevation, so the entry overstated the constraint. Every
session below is on 0.4.191, direct, with the codec forced to H.264 (`data-channel-h264`) unless the row says
otherwise, and the device row is the server's.

| Step | Device row | The session reads |
|---|---|---|
| Deny stored as revision 2 at 19:02:48Z, not delivered | `pending`, last report revision 1 | `encoder=h264_qsv input_path=gpu` |
| Restart 1, 20:13:58Z: the reconnect delivers it | `needs_restart` (`zero_copy_deny`) at 20:14:06Z | `input_path=gpu`: this process started on the old value |
| Restart 2, 20:16:36Z | `applied` at 20:16:53Z | `encoder=h264_qsv input_path=cpu input_reason=denied` |
| The same deny, codec on auto | `applied` | `encoder=av1_qsv input_path=gpu` |
| `none` stored as revision 3 at 20:20:46Z, restart 3 at 20:22:36Z delivers it | `needs_restart` (`zero_copy_deny`) at 20:22:43Z | `input_path=cpu input_reason=denied`: the deny holds |
| Restart 4, 20:24:36Z | `applied` at 20:24:54Z | `encoder=h264_qsv input_path=gpu` |

The third row is AC2. The fourth is the switch's scope: under the same deny, the host's everyday `av1_qsv` kept its
GPU input. The last two show that taking a deny off costs what putting one on does. `encoder_cells_deny` stayed as it
was throughout. Each restart went through the service manager (`exits with code 0; scm relaunches it`) and answered
again in 6.6 to 7.6 s, and the viewer rejoined without a click.

**The switch's reach, for P3.** The deny sat undelivered on an online device for 71 minutes and would have sat longer.
Reconcile-on-connect is the only delivery path, deliberately
(`crates/modules/fleet/src/socket.rs:427`), and no server message restarts a daemon
([`remote-config.md`](../remote-config.md) §7b). So a cell that goes bad stays open on an online host until that host
has reconnected and then restarted, which across a fleet means a pod roll followed by a release's push. With someone
at the device, two `roomler restart`s do it in about a minute. A canary week should be planned on that reach.
Whether the server should also send a changed document to a connected device is a decision for before P3, and was not
taken here.

### 2026-11-02: who runs `hevc_qsv`, and how

An offline read of production `agent_logs`, 2026-10-26 01:30Z to 2026-11-01 22:30Z, from the RC media plan's
independent review. The derivation was re-run for this entry. An hour here is the DC heartbeat's 2 s windows, and a
window that captured under 5 fps is idle.

| Cell | Hours | Idle | Moving | Moving share |
|---|---|---|---|---|
| `hevc_qsv` | 46.97 | 43.88 | 3.03 | 6.5 % |
| `hevc_videotoolbox` | 11.25 | 0.20 | 11.03 | 97.9 % |
| `av1_nvenc` | 5.34 | 0.22 | 5.08 | 95.1 % |
| `av1_qsv` | 4.42 | 1.07 | 3.32 | 75.2 % |
| `hevc_nvenc` | 0.91 | 0.06 | 0.80 | 87.4 % |

By session, `hevc_qsv` ran 47.61 h: 45.30 on four office desktops and 2.30 on CORPLAP-1. Three of the four
advertise CORPLAP-1's encoder list, and the fourth lacks `vp9_qsv`, so the cell matches, but no read names their GPU
or driver. None of the four has a config report on its row, and none holds a desired-config revision above 0, so
nothing has tested whether a push reaches them.

These hours are real use, someone working on an office PC, but the screen changes rarely: 60.6 % of the idle windows
capture under 1 fps, and the encoder spends them on keepalives, which read nothing back. A frame that is captured costs
about what it does under motion. On the builds that report `input_path`, readback reads 0 / 2.66 / 3.76 ms at p10, p50
and p90 in idle windows and 1.88 / 2.90 / 4.10 ms in moving ones. Older builds leave the field at 0, and they hold
51,735 of the 78,980 idle windows, so a read across all builds understates it.

So a keystroke's frame would save about 2.7 ms and a keepalive nothing. §2.1's savings were measured under motion, frame
after frame, and those 43.88 hours capture few frames, so they save little. That is decision 9, and the reason 3d reads
moving and idle windows apart.

### 2026-11-03: P1b on production, AC9's red and AC8's field half

P1b ([#3242](https://github.com/gjovanov/roomler-ai/pull/3242)) reached production in `hosted-20261103-5f12695`,
with both pods Ready at 05:31:50Z. The device is NEO16, which has the remote-config opt-in and no QSV, so no value
below changes a session. Its document manages `exec_enabled` and `ssh_enabled` throughout, and `zero_copy_deny` where
a row says so. Times are the server's clock. The device's clock ran about 4.4 s behind it, and its times are marked.

**AC9, red on 0.4.197.** The image before P1b delivered a document on connect only.

| Step | Device row | The daemon |
|---|---|---|
| Revision 2 (`h264_qsv:gpu`) saved at 01:24:37Z, the device connected | `pending` four minutes later, the last report revision 1 from the day before | not delivered |
| Its service restarted at 01:48:47Z (device clock, not part of this test), and the reconnect delivered revision 2 | `needs_restart` (`zero_copy_deny`) at 01:49:07Z | the new worker started at 01:48:58Z (device clock), before the deny reached its file, so it runs without it |
| The promote's roll: a reconnect at 05:31:19Z, the same worker | `noop`, nothing waiting. The dialog reads "Applied" and "The device confirmed this configuration and it is in force." | the 01:48:58Z worker, still without the deny |

The last row is the defect P1c fixes ([#3283](https://github.com/gjovanov/roomler-ai/pull/3283)). The file already
matched, so the reconnect answered `noop`, and the row said a deny was in force that the daemon did not run.

**AC8, on the new image.** The first save went through the Device configuration dialog. The others sent the
dialog's body to the same endpoint from the page, so they could be timed.

| Case | The save | The report | Read |
|---|---|---|---|
| Connected | revision 3 (`none`) at 05:35:10.587Z | `needs_restart` at 05:35:10.737Z | 150 ms. The socket's only connect is 05:31:19Z, and the worker is the same |
| Two saves 41 ms apart | revision 4 (`h264_qsv:gpu`) at 05:39:04.289Z, revision 5 (`none`) at 05:39:04.330Z | revision 5, `needs_restart`, at 05:39:04.585Z | the device applied twice, in order (its log, 119 ms apart), and its file ends on revision 5's `none` |
| Offline at the save | the socket closed at 05:41:56.177Z for the device's 0.4.198 update; revision 6 (`h264_qsv:gpu`) at 05:41:58.109Z | `pending` until the reconnect at 05:42:12.996Z, then `needs_restart` at 05:42:13.258Z, on 0.4.198 | converged on connect, 262 ms after it |

**Not seen yet: a save on the other pod.** The load balancer routes a request by the tenant id in its path and an
agent's socket by its `tid`, so a tenant's saves and its devices' sockets land on one pod, and every save above was
direct (the PUT and the socket on the same pod, no ctrl-lane line on either). A save takes the ctrl lane only for a
socket left on the other pod, such as a device that reconnected during a roll's gap. That path is integration-tested
(#3242), and AC8 stays open until a save reaches one in the field.

Cleanup: revision 7 (`none`) put the file back to the value the 0.4.198 worker runs, and the device still answered
`needs_restart`. That is the other half of what P1c fixes, seen in the field. Revision 8 stopped managing the key,
and the device answered `noop`.

## 9. Related

[FR-118](FR-118-gpu-resident-frames.md) (#2669) · [FR-77](FR-77-encoder-chroma-matrix.md) ·
[FR-119](FR-119-high-refresh-streaming.md) (#2670) · [FR-130](FR-130-colours-decode-true.md) (#3120) ·
[`docs/encoders.md`](../encoders.md) § GPU-resident frames · [`docs/remote-config.md`](../remote-config.md)
