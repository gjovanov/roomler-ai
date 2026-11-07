# The pump stall watch — how a remote-desktop session says where its time went

The instrument at the heart of [FR-65](fr/FR-65-blocking-work-on-async-runtimes.md):
every pass of the FFmpeg DataChannel pump (`media_pump_ffmpeg_dc`,
`agents/roomlerd/src/peer.rs:6536`) is timed, broken into named phases, and
judged. A pass whose **work** reaches the bar (100 ms) is a **stall**: it is
logged once with its breakdown, counted on the heartbeat, and handed to the rate
governor, whose FR-79 validity gate refuses to learn from a window in which the
agent's own loop was busy ([rate-control.md](rate-control.md#the-validity-gate-fr-79--a-window-is-evidence-about-the-pipe-or-it-is-not)).

It exists because a 2 s blocking encoder open hid for months: the pump already
timed capture, scale, encode and send, and the stall was in none of them. This
page is the instrument's manual — what each phase is, the two rules the verdict
runs on, the bookkeeping that feeds it, what the kill switch does and does not
turn off, how to read a STALL line, how to take a census of them across the
fleet, and what the watch itself costs (measured, FR-65 AC1).

| | |
|---|---|
| Verdict + bookkeeping | `encode::stall` (`agents/roomlerd/src/encode/stall.rs`) — a pure module on the **default** feature set, so `cargo test -p roomlerd --lib` covers it although the pump is behind `ffmpeg-encoder` |
| The pump's sites | `peer.rs:7044` (the watch), `:7056` (the one phase list), `:7534` (the pass boundary), `:10487` (the heartbeat's rebase) |
| Kill switch | `pump_stall_watch` / `ROOMLERD_PUMP_STALL_WATCH=0` (`encode/mod.rs:538`), restart required |
| The bar | `pump_stall_warn_ms` / `ROOMLERD_PUMP_STALL_WARN_MS`, default 100 (`encode/mod.rs:626`) |
| Its own cost | `roomlerd stall-watch-cost` (`stall.rs:592`, §8) |
| The census | `scripts/fr65-stall-census.sh`, a fleet-wide server read of `agent_logs` (§7) |

## 1. A pass, and where its time goes

A pass is one turn of the pump's loop: from the top of the loop to the top of
the next. It is closed and the next opened at the TOP, never at the tail — the
loop has more than a dozen `continue` paths, and closing at the top counts every
one of them without an RAII guard fighting the borrow checker.

```mermaid
flowchart TB
    TOP["loop top: watch.turn_with — close pass N-1, open pass N<br/>(one clock read)"] --> SWAP["swap: adopt a finished background rebuild"]
    SWAP --> STATS["stats: ICE pair re-check (every few s)"]
    STATS --> RECAP1["recapture + close: a persisted transport flip reopens<br/>the capturer and drops the encoder"]
    RECAP1 --> CTRL["ctrl: rc:video-info on the control DC"]
    CTRL --> GATE{"gate: send queue full?"}
    GATE -- "congested: skip production" --> TOP
    GATE -- free --> PACE["pace: sleep to the next cadence slot"]
    PACE --> CAP["capture: next_frame().await"]
    CAP -- "no new frame" --> POLL["poll: 2 ms sleep, or a yield while the display is held"]
    POLL --> TOP
    CAP -- "capture error" --> RECAP2["recapture: backoff + reopen"]
    RECAP2 --> TOP
    CAP -- frame --> SCALE["scale: downscale to the plan"]
    SCALE --> CMD1["enc_cmd: the dims replacement's spec"]
    CMD1 --> OPEN["open (+ close of the old encoder)"]
    OPEN --> CMD2["enc_cmd: keyframe requests"]
    CMD2 --> ENC["encode"]
    ENC --> DCL["dc_lock: the video DataChannel's mutex"]
    DCL --> SEND["send: framing + try_send to the send task"]
    SEND --> HB{"heartbeat due? (2 s)"}
    HB -- yes --> HBK["heartbeat: log the window, then reset it<br/>and REBASE the open pass"]
    HB -- no --> TOP
    HBK --> TOP
```

Seventeen phases. Six are always timed, because the heartbeat publishes them;
eleven exist only for the watch and cost nothing when it is off.

| Phase | What it times | Work? | Timed when the watch is off? | Since |
|---|---|---|---|---|
| `capture` | `capturer.next_frame().await` (`peer.rs:8321`) — mostly **waiting for the screen to change** | no | yes (heartbeat average) | rc.88 |
| `scale` | the downscale to the plan's box (`:8669`) | yes | yes | P7 |
| `encode` | the encode round trip (`:9466`) | yes | yes | rc.88 |
| `send` | framing and the hand-off to the send task (`:9695`) | yes | yes | rc.88 |
| `apply` | a rate/dims change to an encoder that already exists (`timed_apply!`) | yes | yes | FR-65 P0 |
| `open` | opening an encoder, first or rebuilt (`:8836`) | yes | yes | #1279 |
| `pace` | the cadence sleep when the encoder cannot hold the rate (`:8059`) | **no** | no | #1327 |
| `stats` | the ICE pair and LAN-reason stats reads | yes | no | #1327 |
| `ctrl` | the control DC's lock and `rc:video-info` send | yes | no | #1327 |
| `swap` | adopting a finished background rebuild (`:7458`) | yes | no | #1327 |
| `gate` | the backpressure gate, including its skip-and-sleep (`:7942`) | yes | no | #1327 |
| `dc_lock` | waiting for the video DataChannel's mutex after an encode (`:9650`) | yes | no | 2026-10-30 |
| `enc_cmd` | encoder commands other than encode/apply/open: keyframe requests, the dims spec (`:8575`, `:8976`, `:8991`, `:9002`) | yes | no | 2026-10-30 |
| `close` | dropping an encoder — which **joins its thread** (`:7340`, `:7618`, `:8918`, `:9032`) | yes | no | 2026-10-30 |
| `recapture` | rebuilding the capturer: a flip's reopen, the error arm's backoff + reopen (`:7615`, `:8306`) | yes | no | 2026-10-30 |
| `poll` | an empty poll's 2 ms sleep, or the yield while the display is held (`:8153`, `:8280`) | yes | no | 2026-10-30 |
| `heartbeat` | the heartbeat block itself (`:9780`) | yes | no | 2026-10-30 |

⚠️ **`apply` is timed inside `swap`, `gate` and `send` on three paths** (a
background adoption, a backpressure decrease, an overflow decrease), so on those
passes the apply is counted twice and `other` can read 0 while time is still
missing elsewhere. It is rare (a rate move) and was so before FR-65's later
phases; it is recorded here rather than fixed by stealth.

## 2. The verdict — two rules, each paid for in the field

`PassTiming` (`stall.rs:120`) is one pass's breakdown. Two numbers are derived
from it, and both are judgements a reader would "simplify" into something wrong:

| Rule | Code | Why |
|---|---|---|
| **A stall is judged on `work = iter − capture − pace`**, never on `iter` | `work_us` (`stall.rs:327`), `is_stall` (`:374`) | A quiet screen sleeps inside capture. At the 100 ms bar that made idling the MAJORITY of warnings (2026-09-03: seven in 45 s, each `iter 111 / capture 101.9`). A watch that cries about idling is ignored. `pace` is the same idle one door along: at a paced 2 fps it is ~500 ms of one pass. |
| **`other = iter − Σphases` is computed** | `other_us` (`stall.rs:366`) | An overrun whose named phases read ~0 is itself the finding: the encoder open hid there for months, spotted by eye on the fourth occurrence. |

⚠️ The cost of the first rule: a **wedged capture cannot trip the watch**.
`capture_ms` still shows it on the line and `avg_capture_ms` on the heartbeat;
nothing alerts on it. If capture needs an alarm it needs its own threshold.

🔑 **Naming a phase never changes a verdict.** Every phase but `capture` and
`pace` counts as work — exactly as `other` did — so naming one only moves time
out of `other` (`naming_a_phase_moves_time_out_of_other_and_changes_no_verdict`).
That is what made it safe to name six more on 2026-10-30 without a release of
evidence first.

`dominant_phase` (`stall.rs:386`) names the phase that dominated the pass, and
**refuses** to name one when `other` is larger — so a line never blames a phase
for time it did not take (AC2's attribution tests).

## 3. The bookkeeping — `PassWatch`

`PassWatch` (`stall.rs:405`) owns the pass marks, the STALL line's rate limit
(one per 2 s), the window's worst pass and stall count (the heartbeat's
`iter_ms_max` and `pump_stalls`), and a **monotonic** stall total.

```mermaid
sequenceDiagram
    participant L as pump loop
    participant W as PassWatch
    participant H as heartbeat (tail of a pass)
    participant G as governor (FR-79 gate)
    L->>W: turn_with(snapshot) at the top of pass N
    Note over W: close N-1: delta = now - mark, plus carried<br/>stall? count it, line if 2 s since the last
    W-->>L: Some(pass) when a STALL line is due
    L->>G: stalled_passes = stalls_total - last fed
    L->>H: pass N reaches the tail, 2 s are up
    H->>H: log the window, zero capture/scale/encode/send/apply/open
    H->>W: rebase(before, after)
    Note over W: carried += before - mark, mark = after
    H->>W: reset_window() — the total is NOT reset
    L->>W: turn_with at the top of pass N+1 closes N whole
```

### 3.1 The window reset inside a pass (fixed 2026-10-30)

The heartbeat zeroes six accumulators at the **tail** of a pass, after the mark
that pass took at its top. Before the rebase, the close computed `0 − mark` for
each of them, which saturates to 0: the pass that held the heartbeat lost every
microsecond of capture, scale, encode, send, apply and open. Its capture wait
counted as work, its encoder open as nothing, and the pass went to `other`.

🔑 A pass longer than the heartbeat interval **always** holds a heartbeat — the
long pass is what makes it due — so the rule was blind to exactly the stalls
worth finding. `PassWatch::rebase` (`stall.rs:522`) carries what the open pass
had accrued and moves its mark past the reset (`peer.rs:10487`). The census that
found it is §6.

### 3.2 The governor reads a total that never resets (fixed 2026-10-30)

FR-79's gate asks whether the agent's own loop was free in a window
(`governor.rs:1162`, `agent_stalled: stalled_passes > 0`). The pump fed it
`pump_stalls − win_stalls_last`, and the heartbeat zeroed `pump_stalls` but not
the mark: after a window that held a stall, the next verdict read `1 − N`,
which saturates to 0. The first verdict after a reset is the heartbeat pass's
own — the long one. It now reads `PassWatch::stalls_total` (`stall.rs:556`,
`peer.rs:9379`), which only grows.

**What the two fixes change for the controller**, since a stall verdict gates
every rate estimator: a heartbeat pass whose overrun was only a capture wait is
no longer a stall (its window becomes evidence again), and a heartbeat pass
that did stall is no longer hidden from the gate (its window is refused, as it
always should have been). Every other verdict is unchanged.

## 4. What the kill switch turns off

`pump_stall_watch = false` (`ROOMLERD_PUMP_STALL_WATCH=0`, restart required):
no pass marks, no verdict, no window counters, and none of the eleven
watch-only timers — `PassWatch::start` returns `None` without reading a clock.
The six always-on phases keep running for the heartbeat.

⚠️ **It is not a pure instrument switch.** Since FR-79 the verdict is a
controller input: with the watch off `stalled_passes` is 0 on every window, so
the governor's `agent_stalled` gate goes blind and accepts windows it would have
refused. An A/B of whole sessions through this switch measures a different
controller as well as the instrument — which is why AC1's A/B is the harness in
§8 and not a pair of sessions.

## 5. Reading a STALL line

```text
WARN FFmpeg DC pump STALL — one pass exceeded the budget session_id=… codec_label="HEVC"
  constrained=false iter_ms=815.7 capture_ms=10.0 … open_ms=789.0 … dc_lock_ms=0.0
  enc_cmd_ms=0.0 close_ms=0.0 recapture_ms=0.0 poll_ms=0.0 heartbeat_ms=0.0
  other_ms=0.57 work_ms=805.7 dominant=Some(("open", 789024)) target_bps=…
```

- **Read `dominant` first, then `other_ms`.** A named dominant phase is the
  answer; `dominant=None` means the time is in no named phase, and `other_ms`
  says how much.
- **`work_ms` is what was judged**, `iter_ms` is the wall clock. A large gap
  between them is capture or pace — idle by design.
- **One line per 2 s at most**: the heartbeat's `pump_stalls` counts every
  stall, logged or not, and `iter_ms_max` is the worst pass of the window.
- A pass is logged at the top of the NEXT pass, so a line timestamp is the end
  of the stalled pass, and the heartbeat that closes a window reports the pass
  that held the previous heartbeat.

| Shape | Reading |
|---|---|
| `dominant=open`, first pass of a session | the encoder open — P1's make-before-break is the fix, the session has no encoder meanwhile |
| `dominant=encode` | a slow encode on the encoder's own thread (FR-70 M1) |
| `dominant=dc_lock` | the pump waited for the send task's previous frame — see §6.3 |
| `dominant=poll` | the pump asked for 2 ms and got the rest back late: the runtime did not schedule it (FR-65 P2's question) |
| `dominant=close` | a dropped encoder's thread join, on the runtime worker |
| `dominant=None`, `other_ms ≈ iter_ms` | still untimed — the census's job is to find these |
| all six reset phases 0.0, `other ≈ iter`, right after a heartbeat | **pre-2026-10-30 builds only**: the window-reset artifact of §3.1 |

## 6. The 2026-10-30 census — what the logs of three hosts said

Fourteen days of daemon logs (2026-10-17..30) on the three corporate laptops,
read host by host with the census §7 replaced, before any of this shipped.
344 STALL lines:

| Class | CORPLAP-1 | CORPLAP-2 | CORPLAP-3 | Reading |
|---|---|---|---|---|
| `open` | 21 (mean 0.93 s, max 1.4 s) | 122 (0.24 s, 3.0 s) | 37 (0.60 s, 1.5 s) | the encoder open, every session |
| `reset` | 7 (2.6 s, 3.6 s) | 51 (2.2 s, 4.1 s) | 26 (1.5 s, 4.1 s) | **the §3.1 artifact** — real stalls, attributed to nobody |
| `untimed` | 4 (14.7 s, **30.0 s**) | 27 (0.34 s, 1.0 s) | 15 (0.31 s, 1.8 s) | time in no phase named at the time — §6.2, §6.3 |
| `encode` | 10 (0.15 s, 0.23 s) | 0 | 19 (0.19 s, 0.55 s) | slow encodes |
| other phases | 1 swap | 1 capture | 1 ctrl, 1 stats, 1 swap | |

### 6.1 The longest stalls were the ones blamed on nobody

The `reset` lines were emitted within 5 ms of their session's heartbeat with
every reset phase at 0.0 — the §3.1 signature. On CORPLAP-1 six of the seven
came right after a heartbeat whose own fields still held the truth:
`open_ms` 1951–3529 on a session's first pass, i.e. `hevc_qsv` opens of 2–3.5 s
(against 748–789 ms measured on 0.4.60), blamed on nobody.

### 6.2 Thirty seconds in no phase

CORPLAP-1, 2026-10-24: two consecutive passes of **29 995 ms** and
**28 281 ms**, each with `capture_ms` < 0.4, no encode, and everything else in
`other`. Nothing from the pump was logged inside either window. Each ended
10–20 ms after a `keep-busy` state change (`paused reason=button_held`, then
`running`), and the second was followed by the keyframe fallback
(`pending_ms=30244`) and the peer failing. A pass with a near-zero capture and
no encode is an empty poll, so the 30 s was spent in that arm or at the loop
top — exactly the regions `poll` and `close` now name. The mechanism is not
established; the next occurrence will say which.

⚠️ **A dead end, recorded so nobody re-walks it:** keep-busy is not blocking a
runtime worker. Its Windows host does `std::thread::sleep` while it settles and
waits (`keep_busy/host_win.rs:96`, `:134`), but the engine runs on its own OS
thread (`keep_busy/mod.rs:214`), so those sleeps hold no tokio worker. The
correlation stands, unexplained.

### 6.3 The send task holds the DataChannel mutex for a whole frame

Forty of the 46 `untimed` lines (CORPLAP-1 2 of 4, CORPLAP-2 23 of 27,
CORPLAP-3 all 15) have a normal capture and a normal encode and the rest in
`other` — a pass that encoded a frame, then waited. The send task
takes the video DC's mutex in an `if let` scrutinee (`peer.rs:7420`), and Rust
2024 keeps a scrutinee's temporaries alive through the whole body, so the guard
is held across every chunk of a frame's flow-controlled send. The pump's own
`video_bytes_dc.lock().await` after each encode then waits out the previous
frame's send. Checked on the pinned toolchain (1.95, edition 2024): inside such
a body `try_lock` fails; after a plain `let` statement it succeeds.

That would re-couple the pump to the link that rc.106 moved the send off. It is
**named, not fixed**: `dc_lock` (and `heartbeat`, which takes the same mutex)
ship first, and the fix waits for their number, per this FR's first rule.

### 6.4 The first server read, by build (2026-11-03)

One week of `agent_logs` (2026-10-27 02:00Z .. 2026-11-03 01:45Z), read with the
census of §7. The batches carry `agent_version`, so one read holds both sides of
the rebase, which first rode 0.4.192. A heartbeat is about 2 s of session time, so
the rate is what compares two sides of unequal length.

| Host | Build | Heartbeats | STALL lines | `reset` | `untimed` | `open` (mean, max) | Other |
|---|---|---|---|---|---|---|---|
| CORPLAP-1 | before | 4 541 | 35 | 5 | 8 | 13 (0.71 s, 1.2 s) | 8 `encode`, 1 `swap` |
| CORPLAP-1 | after | 23 | 4 | **0** | **0** | 4 (0.58 s, 0.87 s) | |
| CORPLAP-2 | before | 6 195 | 129 | 26 | 5 | 96 (0.22 s, 1.3 s) | 2 `capture` |
| CORPLAP-2 | after | 2 816 | 4 | **0** | **0** | 4 (**2.3 s, 3.0 s**) | |
| CORPLAP-3 | before | 4 667 | 50 | 4 | 0 | 41 (0.50 s, 0.93 s) | 5 `encode` |
| CORPLAP-3 | after | 1 925 | 18 | **0** | **0** | 12 (0.48 s, 0.66 s) | 5 `dc_lock`, 1 `encode` |
| Every device | before | 97 685 | 909 (9.3 per 1 000) | 562 | 60 | 269 | 15 `encode`, 2 `capture`, 1 `swap` |
| Every device | after | 25 477 | 126 (4.9 per 1 000) | **0** | **0** | 86 | 35 `dc_lock`, 4 `encode`, 1 `poll` |

The after is two days old, so it is a first reading, not AC7's. Three things it
already says:

- **The passes `reset` hid are now named.** CORPLAP-2's four `open` lines mean
  2.3 s, where its 26 `reset` lines meant 1.9 s and its `open` lines 0.22 s: those
  are the long opens §6.1 found, now attributed.
- **Most `reset` lines were never long.** 480 of the 562 are under 200 ms, and 456
  of them are one macOS host at 108 ms, once per heartbeat. The reset zeroed
  `capture` too, and `capture` is idle by rule (§2), so a heartbeat pass on an
  idle screen could read as work. A build with the rebase should stop logging
  those at all. That host's after is ten heartbeats, too few to say.
- **`dc_lock` has its first number**: 35 lines, mean 0.52 s, 2.3 s at worst, on
  four hosts. §6.3's mutex is real in the field.

## 7. Running the census

The census is a fleet-wide server read of `agent_logs`. Nothing runs on a device.
`roomlerd` writes each STALL line once, and the uploader carries it to the server
with every other event, so the read covers every daemon and RC worker, on every
OS and in every org, including a host nobody can reach. A sweep of host logs covers
only the hosts it can reach, a biased sample
([rate-control.md](rate-control.md) explains why).

```mermaid
flowchart LR
  L[STALL and heartbeat lines] -->|every event| U[uploader, 5 s batches]
  U --> A[(agent_logs, 7 days)]
  A -->|one day per query, streaming| C[fr65-stall-census.cjs under mongosh]
  L -->|rolling log file| F[--from-logs, under Node]
  C --> R[one row per class, per device, per build]
  F --> R
```

```bash
scripts/fr65-stall-census.sh --mars --yes                     # the last 7 days, every device
scripts/fr65-stall-census.sh --mars --yes --rows              # + one JSON line per STALL, classified
scripts/fr65-stall-census.sh --mars --yes --agent <hex> --since 2026-10-27T02:00:00Z --until 2026-11-03T02:00:00Z
scripts/fr65-stall-census.sh --mars --yes --probes            # the caps-probe census (FR-65 AC6)
node scripts/fr65-stall-census.cjs --from-logs <file>...      # the same classifier over log files you hold
node scripts/fr65-stall-census.cjs --self-test
```

It prints, per device and build, the lines per class of §5's table, and the same
totals before and after 0.4.192. A week of the whole fleet takes about 10 s.

- **It is bounded by construction**, because an unbounded read inside the Mongo
  container once OOM-killed production: at most 8 days, one day per query, each
  with `maxTimeMS` and `allowDiskUse: false`, streaming stages only, and the fold
  keeps counters and each session's last heartbeat. `--yes` is required, and
  the Mongo pod's restart count is printed before and after. A count that moved
  makes the run exit 3, and the run is harm, not data.
- **`--rows` also carries, for a `reset` line, the heartbeat just before it**,
  where the lost attribution can still be read. Keep the output of a read you
  mean to compare later: the lines expire in 7 days and the rows do not.

What it cannot see, and what it prints about that:

| Limit | Why | What the read does |
|---|---|---|
| **7 days**, where the host census read 14 | the TTL on `agent_logs` | A window is at most 8 days. A before and an after are compared at the same length, or by rate per 1 000 heartbeats. |
| **The uploader can lose lines** | a full channel drops and counts (`note_dropped_lines`, `crates/agent-core/src/logs_upload.rs:148`); a shed batch (`:419`) and a refused one are counted nowhere | Each row prints `dropped`, `hb_gaps` (a session's heartbeats over 10 s apart, with the seconds missing) and `counted`, the sum of the heartbeats' `pump_stalls`. `counted` under `stalls` means heartbeats were lost. |
| **`reset` needs the heartbeat before it** | a lost heartbeat would turn a `reset` line into `untimed` | Such a line is counted again as `reset_no_hb`. |
| **A stamp is in milliseconds** where the file has microseconds | `DateTime::now()` in the upload layer (`logs_upload.rs:190`) | 4.7 ms can read 5, so the fleet read takes ≤ 5 ms: an edge line is called `reset`, the class AC7 wants at 0. |
| **Silence is not health** | only the FFmpeg DC pump has the watch (§9) | `heartbeats` is the denominator: a row with none ran another pump, or no session. |

⚠️ **The two ends agree, and this is the check to repeat if the classifier
changes.** On a test Windows host (NEO16), one week of the same lines, 2026-10-27
02:00Z .. 2026-11-03 01:45Z:

| Compared | Lines | Matched | Class disagreements |
|---|---|---|---|
| The PowerShell census, as a plain file, against the fleet read | 64 and 72 | 64 | 0 (35 `open`, 24 `reset`, 3 `untimed`, 2 `encode`) |
| The PowerShell census against `--from-logs` over the same files | 64 and 64 | 64 | 0 |

The eight lines on the server alone are all `open`, on 2026-10-28, from a daemon
whose log file was in neither directory that census read. The server held more,
and it lost none of the 64. A matched line's two stamps were at most 1 ms apart.

⚠️ **`--probes` reads the same population only if the lines ship**, and they do:
on that host and week the fleet read gives 58 probe lines, all within 60 s of a
daemon start, 0 at steady state. The files hold 199 more, from test runs that
never uploaded under this device, so a file count and a fleet count of one host
can differ by what ran outside its daemon.

The PowerShell census itself is gone: its last copy is
`scripts/fr65-stall-census.ps1` at `06989b3fd`, with the invariant culture and
file-sharing traps it paid for.

It is FR-65 AC7's "same instrument": the before of a change to the watch is the
build without it, the after is the build with it, over windows of the same
length.

### 7.1 Three more modes: forced rekeys, swaps, the runtime canary

Three FR-65 criteria have their number in the fleet's logs rather than in a STALL
line. Each gets a mode of the same read, bounded the same way: the server keeps
only the mode's own lines and projects only the fields its fold reads.

| Mode | Criterion | Lines it keeps | What it prints, per device and build |
|---|---|---|---|
| `--rekeys` | AC5, and P3's before and after | `overlay: net-change — revalidating direct carrier now` (a poke, one per carrier), `overlay: network change — re-asserting peer routes now` (a route wave), `netstate: network changed` (a delta), `… carrier failed active revalidation` | pokes, per minute at the peak and at p99; sweeps (pokes under 1 s apart, one network event poking every direct carrier at once); re-armed pokes; Major sweeps; what P3's gate would have kept (`gate_replay_keeps`); waves, material apart; deltas, Major apart; failed revalidations, direct and relay |
| `--swaps` | AC3 | `FFmpeg DC pump heartbeat` | per encoder and `constrained`: heartbeat windows, then the windows where `dims_swaps` rose (a downscale-tier change or its undo) and the windows where the size moved with `dims_swaps` still (the source changed size), each with its worst `iter_ms_max` and how many passed 100 ms and 250 ms |
| `--canary` | AC4 | both `runtime canary:` lines, STALL, heartbeat, `agent starting` | the canary's 10-minute summaries (those with a 100 ms wake apart), wakes over 10, 50 and 100 ms, gaps, p99 (median and worst), the latest wake, warnings; then the STALLs, how many a summary covered, and how many of those were in 10 minutes with a 100 ms wake (`runtime_late`) |

```bash
scripts/fr65-stall-census.sh --mars --yes --rekeys            # forced rekeys, every device, the last 7 days
scripts/fr65-stall-census.sh --mars --yes --swaps --rows      # + one JSON line per swap or size change
scripts/fr65-stall-census.sh --mars --yes --canary
node scripts/fr65-stall-census.cjs --from-logs --rekeys <file>...
```

`gate_replay_keeps` replays P3's gate (`lifecycle::netchange_action`) from the log
alone. A poke line carries no `material`, so a delta line in the 10 s before a sweep
stands for it:

```mermaid
flowchart TD
  P[a poke] --> S{under 1 s after the last poke?}
  S -->|yes| J[the same sweep, the same verdict]
  S -->|no| M{from_major, or a delta line in the 10 s before?}
  M -->|yes| K[kept, and the quiet starts again]
  M -->|no| Q{the last kept sweep 60 s ago or more?}
  Q -->|yes| K
  Q -->|no| D[dropped by the gate]
```

On a build without the gate it says what the gate would shed. On a build with it,
every poke already passed the gate, so `gate_replay_keeps` should equal `pokes`;
a shortfall is a disagreement between the replay and the gate. The replay reads
stamps at the server's millisecond: its 60 s quiet ends on a sweep of the 3 s
chatter itself, and the file's microseconds flipped 19 of the verdicts below.

`--canary` covers a STALL when a summary of the same device follows it within its
10 minutes with no `agent starting` between. A build without the canary covers
nothing, so `covered=0` there is no reading. AC4's "frame age unchanged or better"
is not in this mode: it is the heartbeat's `viewer_age_ms`, read beside it.

⚠️ **The two ends agree here too.** The same host as above, 2026-11-02, one day:

| Mode | Host files (`--from-logs`) | Fleet read | Difference |
|---|---|---|---|
| `--rekeys` | 14 140 pokes, 1 654 sweeps, 4 004 kept by the replay, 1 987 waves, 7 deltas, 10 + 1 failures | 14 128 pokes, 1 653 sweeps, 4 004 kept, 1 987 waves, 6 deltas, 10 + 1 failures | one sweep of 12 pokes and one delta, both on 0.4.197, missing from the server with `dropped=0`: a batch lost where the uploader counts nothing (the table above) |
| `--swaps` | 149 heartbeats, one dims swap at 196.4 ms | the same | none |

The first fleet read on `--rekeys`, 2026-10-27 04:10Z .. 2026-11-03 04:10Z, every
device: 208 210 pokes, of which the gate would have kept 64 138; 33 710 route
waves; 1 399 direct and 786 relay revalidations failed. It ran in about 11 s, and
the Mongo pod's restart count read 1 before and after. That is P3's before.

## 8. What the watch costs (FR-65 AC1)

> *"An instrument that costs what it measures is worthless."* AC1 asks for the
> overhead **measured** at under 1 % of the frame budget by an A/B with the
> watch disabled, on real hardware — explicitly not the arithmetic ("an
> `Instant::now()` is ~20–40 ns, and there are N of them") the switch's own
> description used to quote.

`roomlerd stall-watch-cost` (`stall.rs:592`, dispatched in `app.rs:1308` before
any runtime, config, log, lock or updater) runs the pump's per-pass watch work
through the **real** `PassWatch`, once enabled and once disabled, rounds
alternating which arm goes first. Everything else in a pass is the same in
both arms, so the difference is the watch.

| Cell | Per pass |
|---|---|
| `steady` | the pass boundary + `swap` + `gate` + `dc_lock`: what every pass enters while frames flow (7 clock reads ON) |
| `worst` | the boundary + every watch-only phase on every pass (23 clock reads ON) |
| `warm` | nothing between the watch's operations — the precise figure |
| `cold` | a 1 MiB walk between every one of them — the pump runs capture and an encode there, and a tokio worker may move it to another core |
| `control` | `steady-warm` with a calibrated spin **injected** into the ON arm at three times the bar; at least half must be measured back, or the verdict is INVALID |

**The bar is 1 % of a frame at the pump's highest rate**: `frame_rate::CHOICES`
tops out at 144 fps, so 1 % of 6.94 ms = **69.4 µs a pass** — not the 167 µs a
60 fps budget would give.

**Measured on NEO16, 2026-10-30 04:54Z** — the dev box (Windows 11), a release
build of this branch, the desktop in normal use (the interleaving absorbs that
load). Four runs of 31 rounds:

| Cell | Watch cost per pass, median (range of the four runs) | Worst p90 in any run | Worst p90, % of a 144 fps frame |
|---|---|---|---|
| `steady-warm` | 225 ns (207–242) | 260 ns | 0.0037 % |
| `steady-cold` | 300 ns (275–329) | 423 ns | 0.0061 % |
| `worst-warm` | 693 ns (662–713) | 743 ns | 0.0107 % |
| `worst-cold` | 854 ns (830–992) | **1 090 ns** | **0.0157 %** |
| `control` | 208.3 µs injected, 208.9–209.2 µs measured back (1.00×) | — | resolved in every run |

One `Instant::now()` costs 28–30 ns here, so the steady cell's seven clock
reads are most of its 225 ns. A first session at 02:29Z, with the box's clock
read at 24–27 ns, put the worst p90 at 869 ns: the host's state moves these
numbers more than the watch does.

**Per second — what "1 % of the frame budget" means once a frame takes several
passes.** The pump re-polls an idle screen every ~2 ms (rc.99), so it makes
more passes than frames. The field's rate, bounded from the heartbeat's frame
counters (`Δframes_empty + Δframes_encoded + Δframes_skipped_backpressure +
Δdc_unopen_drops` per window, which over-counts a keepalive pass) over 14 days
and 36 357 windows: at most **322 passes/s** (NEO16, 144 fps); 299, 314 and 296
on the three CORPLAP hosts (p99 273–282), up to 10.5 passes per frame interval
at 30 fps. At the worst cell's p90 that is 322 × 1 090 ns = **0.35 ms of watch per
second, 0.035 % of the frame budget — 28× under the 1 % bar**, with every factor
measured on the host it describes. From this build on the heartbeat carries the
rate itself (`passes`), so the next read needs no bound.

**The positive control is the "fail first".** The current release cannot run
this harness at all, so the failing run is the control: with 208 µs injected
into the ON arm the same instrument reads 208.9–209.2 µs, three times the bar — a
watch costing that much would FAIL (`the_verdict_needs_the_control_to_resolve`
pins the verdict side).

⚠️ **A dead end, recorded:** the first version differenced whole cold passes,
as the warm cells do. A cold pass is ~80–280 µs of walking whose time varies by
microseconds, so the watch came out at −6.4…+5.7 µs medians and p90s up to
37 µs — a bound set by noise, which multiplied by 322 passes/s is 1.2 % of each
second and proves nothing. The cold cells now bracket only the watch's own
operations after each eviction walk (`pass_timed`, `stall.rs:822`). And the
first control was a CALIBRATED LOOP COUNT: it came back at 0.83–1.16× over
eight runs, because a core that changes its clock between calibration and run
shrinks a step count — on a shared CI runner that is a flake waiting to happen.
The control is now a busy-wait for a fixed wall-clock time, and reads 1.00×.

```bash
roomlerd stall-watch-cost            # ~15 s, text; exit 0 PASS, 3 FAIL, 4 INVALID
roomlerd stall-watch-cost --json     # for a script
roomlerd stall-watch-cost --quick    # ~1 s smoke
# on a test device:
roomler exec <device> -- "C:/Program Files/Roomler/roomlerd.exe" stall-watch-cost
```

## 9. Known limits

- **Only the FFmpeg DC pump has the watch.** The VP9-444 pumps and the RTP
  track path do not; a host whose sessions ran them logs no STALL lines, which
  is silence, not health.
- **`apply` nests inside `swap`, `gate` and `send`** on three rare paths (§1).
- **A wedged capture cannot trip it** (§2).
- **The census sees what was uploaded** (§7): 7 days, and less than that on a
  host that lost lines.

## 10. The runtime canary — what the watch cannot see (FR-65 P2)

The watch times the pump's own passes. A task that never got scheduled is
invisible to it: if something holds a worker of the daemon's runtime (a
synchronous FFI call, a guard held across an `.await`, a poll that never
yields), every task queued behind that worker waits, and the pump's next pass
merely starts late. FR-65 P2 asked for an instrument that measures that symptom
itself, and FR-70 AC1 asks for the same one. This is it.

| | |
|---|---|
| Code | `agents/roomlerd/src/runtime_canary.rs`, spawned beside the resource watch (`app.rs`) |
| What it does | asks to wake every 10 ms on the daemon's main runtime and records how late each wake came |
| Summary | one INFO line every 10 minutes: `runtime canary: how late a 10 ms timer woke, over 10 minutes` |
| Warning | one WARN at most a minute when a wake comes ≥ 100 ms late (the watch's bar): `runtime canary: a 10 ms timer woke late, so the runtime did not schedule its tasks` |
| Kill switch | `ROOMLERD_RUNTIME_CANARY=0` |
| Its cost | one timer: 100 wakes a second, each a clock read and a histogram increment |

```mermaid
flowchart LR
    T[sleep until due] --> W[woke: late = now − due]
    W --> G{late ≥ 30 s?}
    G -- yes --> GAP[gaps += 1<br/>the host slept]
    G -- no --> H[histogram, max]
    H --> X{late ≥ 100 ms?}
    X -- yes --> WARN[WARN, at most once a minute<br/>unwarned counts the rest]
    X -- no --> N
    WARN --> N[due = now + 10 ms]
    GAP --> N
    N --> T
```

**The summary's fields.** `wakes`; `p50_ms`, `p99_ms`, `p999_ms`, each the
histogram bucket bound (1, 2, 5, 10, 20, 50, 100, 250 ms) under which that share
of the wakes fell, or the latest wake when that share is past 250 ms; `max_ms`,
exact; `over_10ms`, `over_50ms`, `over_100ms`, wakes later than each; `gaps`,
wakes 30 s or more late. An idle runtime reads `p999_ms=1` and every `over_*` at
0. P2's measured case, a 13 ms synchronous encode on a worker every frame at
30 fps, would read `p99_ms=20` with a third of the wakes in `over_10ms`
(`a_worker_held_13_ms_a_frame_shows_in_the_tail`).

**Why it measures from `now`.** The next deadline is set from the wake, not
from the missed one, so a 2 s stall counts as one wake 2 s late, not as 200
wakes of which 199 look on time and water the tail down.

**Why a gap is not a stall.** On Windows the daemon holds a 1 ms timer
resolution for its whole life (`win_timer`), so a late wake there is the
scheduler, not the 15.6 ms tick. But a laptop that sleeps wakes the canary hours
late. A wake 30 s or more late is counted in `gaps` and never warned: a runtime
starved that long is the watchdog's to catch (its budgets are 30–90 s).

**Reading it beside the census.** A STALL whose pass shows no phase and a
canary WARN within the same second is the runtime's; a STALL with a dominant
phase and an on-time canary is the pump's own work. A fleet read of both is
what AC4 asks for.
