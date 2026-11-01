# FR-16: Systematic remote-desktop quality benchmark

Status: **proposed** (2026-08-27); **amended 2026-10-13**, so that L0 (an in-process
harness) and L4 (input-to-photon) come first ([§ Amendment](#amendment-2026-10-13--l0-and-l4-come-first)).
L0 merged; L4's agent and viewer halves built and run end to end on a release build. **L1
built 2026-10-30**: `roomlerd encoder-smoke --bench` prints a per-host encoder cost table
whose verdict reads `fallback` when hardware was asked for and software answered
([testing.md](../testing.md#the-l1-encoder-bench-fr-16)). Its fleet run through `roomler exec`
began on 2026-10-31 on 0.4.191: [4 of the 11 online hosts](#l1-fleet-run-2026-10-31), and it
caught a real fallback on two of them.
Tracking issue: `FR-16` (#798). Sibling of FR-17 (#799, the architectural fix); the
measurement half of the FR-1 program, and the instrument of the
[StarDesk teardown program](../plans/stardesk-teardown-program.md).

## Goal

Turn "remote desktop quality" from a set of anecdotes into a **repeatable, scored
matrix** over the dimensions that actually vary: codec (AV1 / H.265 / H.264 / VP9 /
VP8), chroma (4:2:0 / 4:4:4), encode and decode path (HW / SW), transport (direct /
relay), resolution and fps, and the device pair. Without it, every optimisation is
argued from one person's impression of one drag.

## Why the current method cannot answer the question

Field session 2026-08-27, six machines on ONE desk sharing one Wi-Fi (neo16, CORPLAP-3[vpn],
CORPLAP-1[vpn], CORPLAP-2, MacBook, WINHOST-J), operator driving each pair by hand:

| pair | result |
|---|---|
| neo16 → CORPLAP-3 | fine |
| CORPLAP-1 → CORPLAP-3 | 60 ms baseline, 600+ ms spikes |
| CORPLAP-1 → WINHOST-J | 1–2 s frozen at drag start, then ~80 ms, "not smooth" |
| MacBook → CORPLAP-1 (H.265) | **20 000+ ms**; VP9 HW better but still sluggish |

Three properties of that data make hand-testing structurally unable to compare cells:

1. **Drag speed is a first-order input.** The operator's own observation: *"if I do it
   slowly it remains around 60 ms, speeding up jumps to 300 ms."* Any A/B between two
   codecs is confounded by how fast the mouse moved.
2. **The interesting statistic is the tail, not the mean.** "Fine" vs "sluggish"
   tracked spikes and freezes; a session averaging 80 ms with 600 ms excursions reads
   as broken while a flat 80 ms reads as acceptable.
3. **The cells interact.** Codec choice changes encode cost AND frame-size
   distribution AND therefore loss exposure on a lossy relay — so a codec verdict from
   one pair does not transfer to another pair with a different carrier.

Calibration to preserve — the operator's own ladder, which every score must reproduce:
**5–15 ms great · 50–75 ms medium · >100–120 ms sluggish.**

## Design — three independent layers, cheapest first

Each layer is separately useful and separately runnable. Most of the combinatorial
space is decided by L1 and L2, which need no session at all.

**L1 — host encoder profile (no network).** Extend the existing `encoder-smoke`
subcommand into a bench: for each (codec × chroma × resolution × target bitrate ×
synthetic motion profile), report encode-ms p50/p95, produced-vs-target bitrate
accuracy, IDR-size and delta-size distributions, and **which path actually ran**
(the "advertises HW, silently falls back to SW" class is invisible today). Ships as a
CLI subcommand so the whole fleet profiles itself through `roomler exec`. Output is a
per-host table: what this machine's encoders actually cost.

**L2 — path profile (per pair).** Extend `roomler diag pair`: carrier and how it was
chosen, RTT and jitter distribution, achievable goodput over the same transport the
video will use, loss/retransmit indication. Run N×N across the fleet → a pair matrix.
No video, no session.

**L3 — end-to-end (per pair × codec × settings).** The expensive layer, so it runs only
on the cells L1/L2 say are interesting.
- **Deterministic motion**: drive the agent's `synthetic-frame-source` with a known
  scene (a rectangle traversing at a fixed px/s), so drag speed becomes a controlled
  parameter with several fixed levels instead of a human variable.
- **Headless transport bench**: `roomler rc-bench <peer> --codec … --duration …` opens
  a real RC session and measures the transport half — wire timestamp → arrival — with
  **no decoder involved**. This is where the field data says nearly all the variance
  lives, and it removes the browser from most of the matrix.
- **Decode/paint cell**: a small Playwright job against the real viewer for the half
  the headless bench cannot see (decode cost, paint, HW-vs-SW decode), driven through
  the existing localStorage/URL knobs.

## Metrics and scoring

Per run: age p50/p95/p99, freeze count (paint or arrival gaps > 200 ms), time-to-first-
frame, sustained fps, delivered bitrate, `send_wait` p95/p99, skip and IDR counts. The
distribution is the deliverable — a mean hides exactly the excursions the operator
reacts to.

One scalar score per cell, calibrated to the ladder above, so a matrix run produces a
ranked table and a regression gate rather than a wall of numbers.

## Amendment 2026-10-13 — L0 and L4 come first

**Why.** StarDesk (GearUP, 2025) advertises "under 50 ms" remote-desktop latency. Roomler
cannot say whether it beats, matches or loses to that: no layer above measures **input to
photon**. Age starts at capture, so input coalescing, injection, the host compositor and the
capture gate are invisible to it. Every perf FR of the
[StarDesk teardown program](../plans/stardesk-teardown-program.md) needs a red-first number
before its first change, so the order of this FR is inverted: **L0 and L4 first**, then
L1–L3. Anchors are master `c689d26a0`.

### L0 — an in-process harness that CI runs

- **Agent side.** `AgentPeer::new` (`agents/roomlerd/src/peer.rs` L293) needs no API
  server. Its answer comes from `handle_offer` (L1398), trickle goes through
  `add_remote_candidate` (L1438), and teardown is `close()` (L1536); a dropped peer frees
  nothing. One seam is added: a hook on the `SettingEngine` the constructor builds
  (L369), so a test can call `set_vnet`
  (`crates/vendored/webrtc/src/api/setting_engine/mod.rs` L248).
- **Impairment through webrtc-util vnet,** not a UDP proxy. vnet gives a min delay, jitter,
  per-chunk loss filters and a token bucket, inside one process, deterministically.
- **Controller side.** A webrtc-rs peer, H.264 depacketized into `openh264::decoder` (already
  wrapped in `agents/roomlerd/src/recording/decode.rs` L59–68). With no FFmpeg in CI, video
  rides the RTP track (`write_sample`, `peer.rs` L2913). The data-channel cell waits for a
  decoder binding, and the spec says so.
- **One clock.** The synthetic source records `counter → Instant`; the decoded marker gives
  the counter back. Latency is `now − t_capture` on the same clock.
- **It runs in CI** the way `control_dc_record` already does: one more explicit
  `cargo test -p roomlerd --features openh264-encoder,synthetic-frame-source --test l4_latency`
  line next to `.github/workflows/ci.yml` L686–689. A test file nobody names runs in no lane.
- **Cells** run at the synthetic source's default 320×240 and the software pump's 30 fps.
  CI builds are unoptimised and openh264 pegs a core long before 1080p, so larger sizes
  (`ROOMLERD_SYNTHETIC_SIZE`) are for local and field runs. 1080p, 4K and high refresh are
  field cells.
- **Assertions that can fail.** The bounds are deltas and generous ceilings, because the
  absolute numbers depend on the machine:
  - a clean link decodes ≥ 0.9× the target fps, with no freeze and p50 under a debug-build
    ceiling;
  - a 100 ms vnet delay raises p50 by ≥ 80 ms, so the metric demonstrably sees the network;
  - 3 % loss (with 20 ms delay and 5 ms jitter) leaves a session that still decodes; its
    freezes and IDRs are printed;
  - **a planted regression** (a 50 ms capture-stage hold) raises p50 by ≥ 20 ms. A benchmark
    that cannot be made to fail is not a gate.

### The synthetic source grows up

`agents/roomlerd/src/capture/synthetic_backend.rs` today paints 320×240 and clamps fps to
1..60 (L57). In bench mode it gets:

- **Size and rate:** parametric W×H up to 3840×2160, and 1–240 fps. The clamp lifts only in
  bench mode.
- **Deterministic scenes:** a rectangle crossing at N px/s, and scrolling text. Drag speed
  becomes a controlled input instead of a human one, the problem this FR started from.
- **A marker that survives compression:** luma-only 16×16 blocks (Y 16 / 235), a 12-bit
  sequence number and a 4-bit CRC in a 2×8 grid, with an 8 px quiet margin. It survives
  4:2:0, high QP and a 4× downscale. Today's 1-px counter strip does not.

### L4 — input to photon in the real viewer

- **The probe.** The viewer sends `{t:"probe", seq}` on the input data channel. The agent in
  bench mode paints `seq` into a **second marker, bottom-right**, in every synthetic frame
  that follows; the frame's own number stays bottom-left. The decode worker reads the marker
  from each decoded frame and records **paint − send on the browser's own clock**, so no
  clock offset is needed. The agent half is built, and `l4_latency.rs` gates the round trip
  in process with its `probe` and `probe50` cells ([testing.md](../testing.md#l4-in-process-the-bench-probe)).
- ⚠️ **Probes go out at a jittered interval.** A period that is a multiple of the frame
  interval (250 ms is 15 frames at 60 fps) lands every probe on the same phase of the capture
  clock. The run would then report one arbitrary point of the wait for the next capture
  instead of its distribution, and two runs could differ by up to a frame interval for that
  reason alone. That follows from the arithmetic; it is not a measurement. The in-process
  harness jitters for the same reason, and its first run did not isolate the effect (field
  log, 2026-10-13).
- **The sample** is taken from `frame.clone()` with `copyTo({rect})` of the 144×48 corner,
  so the paint path is untouched. A GPU-backed frame reads back whole, about 1–2 ms at
  1080p. So the worker samples **only while a probe is armed**, one frame at a time in paint
  order, and an unanswered probe expires after 2 s. The design first said "one frame in N";
  that would have credited each probe to a frame up to N−1 frames late. On the plain
  `<video>` path the same read goes through a small canvas inside
  `requestVideoFrameCallback`, stamped with its `presentationTime`
  (`ui/src/composables/rcBench.ts`).
- **Gated at every entry point:**
  - the agent's marker painter exists only in the synthetic source;
  - `probe` is refused unless `ROOMLERD_BENCH=1` **and** the build has the feature;
  - the viewer reads `roomler-rc-bench` only when the hello advertises `bench`;
  - the worker samples only when its init says `bench: true`.
- **Upload** goes through FR-22's path (`ui/src/composables/rcConnectTimingUpload.ts`,
  target `rc.l4`, timing metadata only), so fleet runs are queryable in `agent_logs`.
- **The host path.** `roomlerd probe-target` is a small topmost window that flips the marker
  on **real OS input**. It is the only cell that includes injection and the host's own
  compositor. Field-only. **Built** (`agents/roomlerd/src/probe_target.rs`): it sits flush in
  the primary monitor's bottom-right corner, 144 × 80 physical px, with 16 cells above the
  marker area. The viewer's `target` mode (`roomler-rc-bench=target`) probes with pointer
  moves onto the cells, aimed with `rc:video-info`'s native size. The window paints the cell's
  number where the bottom-right decoder reads. It runs on every Windows build, release
  profiles included, through `windows-sys`; Linux and macOS hosts bench with a
  synthetic-source build ([testing.md](../testing.md#l4-on-a-release-build-roomlerd-probe-target)).
- **Nightly.** `rc-bench.spec.ts` runs in the pwrunner sidecar against the synthetic e2e
  agent (`Dockerfile.agent-e2e`) and records the L4 trend.

### The StarDesk comparison cell

The same instrument is used on the operator's spare, non-fleet Windows box with an NVIDIA
GPU, for both products, on the same host, client and network. It records:

- click-to-photon from a 240 fps camera (±4 ms) or a HID-click + photodiode rig;
- `nvidia-smi` encoder-session stats (sessions, average fps, average latency);
- packet headers only;
- what each product installs, and how its privacy mode, auto-lock, follow-display and mouse
  auto-switch behave.

It is black-box and clean-room: nothing is decompiled. ⚠️ **Results stay in the private
docs repo.** [`docs/compare/README.md`](../compare/README.md) rule 3 is "We do not benchmark
competitors". Engineering may use the numbers; the public pages never print them.

### Phases (amended)

| P | Scope | Kill switch | Status |
|---|---|---|---|
| L0 | The harness, the synthetic source, the CI line, the planted-regression control | test-only | **merged** #2097 (`67d3ca9e8`): `agents/roomlerd/tests/l4_latency.rs` + the marker and clock in `capture/synthetic_backend.rs` + the `peer::l0` seam; local and CI runs in the field log |
| L4 | The probe, the worker sampler, the upload, `probe-target`, the nightly spec; baselines on a direct and a relay pair | `ROOMLERD_BENCH` unset; the build feature; the viewer knob `roomler-rc-bench` | **agent half built**: `synthetic_backend::probe` + the bottom-right marker, the input-handler hook in `peer.rs`, `bench` in `AgentCaps.video`, the `probe`/`probe50` cells. **Viewer half built**: the TS marker decoder, the worker sampler in all three decode workers, the `<video>` sampler, the jittered prober, the `rc.l4` upload, the server's parse lock. **`probe-target` built**: the window (`probe_target.rs`) and the viewer's `target` mode. **End to end on a release build: done** against `probe-target` on vmtest guests (the field log, 2026-10-13; FR-98 P5's cells used it on 2026-10-16, on the RTP track and the DataChannel). Owed: the nightly spec, which needs the agent-e2e pods in bench mode (`ROOMLERD_BENCH=1` in `scripts/e2e-k8s/overlay-template/statefulset-agent-e2e.yaml`, and an agent-e2e image from 0.4.133 or later, built by hand on the build host), and the baselines on real hosts |
| T | The StarDesk comparison cell on the spare box (private results) | — | — |
| L1 | `roomlerd encoder-smoke --bench`: each cell opened through the constructor a session calls, a synthetic motion profile encoded on one thread holding its clock, one row per cell with encode ms p50/p95, produced vs target bitrate, IDR and delta sizes and the encoder that opened; the `fallback` verdict | the flag: `--bench` is opt-in, and every `--bench-*` flag requires it, so the release lane's `encoder-smoke --encoder hardware` runs the plain smoke as before | **built** (2026-10-30): `agents/roomlerd/src/encode/bench.rs`, the painter moved to the always-compiled `capture/synthetic_scene.rs` with its motion profiles, the flags flattened into `EncoderSmoke` (`app.rs:451`) and dispatched by `smoke_mode` (`app.rs:1977`). The container run is in the field log, the fleet run in [§ L1 fleet run](#l1-fleet-run-2026-10-31). Owed: the hosts it has not reached (AC1) |
| L2–L3 | As designed above | — | — |
| D | Docs | — | L0 and L4: `docs/testing.md`, done. L1: `docs/testing.md` § The L1 encoder bench, done |

## Acceptance criteria

- [~] L1 runs on every fleet host through `roomler exec` and produces a per-host
      encoder cost table, including a host where an advertised HW encoder actually
      falls back (proving the check can fail).
      **The code half is done** (#3155): `roomlerd encoder-smoke --bench`
      (`agents/roomlerd/src/encode/bench.rs`), with the verdict read from the opened encoder and
      pinned through a fake opener (`the_verdict_is_read_from_what_opened`,
      `a_run_through_the_seam_prints_one_verdict_per_row`), and a release build's table from a
      container in which `dc:vp9:444` read `fallback` (field log, 2026-10-30). **The fallback
      half is met (2026-10-31, on 0.4.191):** two Windows hosts advertise `mf-h264-hw`, and on
      both the bench opened Microsoft's software MFT and `rtp:h264` read `fallback`, beside
      rows that opened `hevc_qsv` and `h264_qsv` in hardware
      ([§ L1 fleet run](#l1-fleet-run-2026-10-31)). **Coverage is partial, so the box stays
      `[~]`:** 4 of the 11 online hosts ran it, of the 20 rows the server lists. zeus and
      MacBook-1 can run it on a quieter night. **Owed:** those two, and a way to read
      CORPLAP-1, CORPLAP-2, CORPLAP-3, NEO16 and NEO16-WSL, which `roomler exec` does not
      target.
- [ ] L2 produces an N×N pair matrix with carrier + RTT + goodput.
- [ ] L3 reproduces a known-bad cell (MacBook → CORPLAP-1 H.265) and a known-good cell
      (a direct pair) with scores on opposite ends of the ladder — the harness must
      first REPRODUCE the field results, or it is measuring something else.
- [ ] Two consecutive runs of the same cell agree within a stated tolerance
      (repeatability, without which nothing can be A/B'd).
- [ ] The FR-17 before/after is reported as a distribution from this harness.
- [x] **L0** runs in CI as a named `--test`. It is green on clean links, and **red** under
      the planted 50 ms capture gate (the run that proves it can fail is linked).
      Evidence: CI's recorder job names `--test l4_latency` since #2097 (`67d3ca9e8`); green
      on the runner (field log); red with the gate zeroed (field log, `gate50: p50 rose by
      0.4 ms, want ≥ 20`).
- [ ] **L4** puts a known-bad relay pair (CORPLAP-1 → neo16) and a known-good direct pair
      (neo16 → MacBook) at opposite ends of the ladder. Two runs of one cell agree within
      ±10 % at p50. On one cell, L4 agrees with a 240 fps camera within the viewer's
      display latency.
- [ ] The StarDesk comparison cell is recorded in the private docs repo, with method and
      date, and never published (compare rule 3).
- [x] **Docs:** `docs/testing.md` gains the L0 harness and the L4 bench (a mermaid diagram
      of the probe → marker → sample loop, how to run each, how to read `rc.l4` in
      `agent_logs`), linked from `docs/README.md`.
      Evidence, read on master `4f2bccc53` (2026-10-30): § The L0 latency harness
      (`docs/testing.md:271`, its mermaid `:278`, how to run it `:321`); § L4 in process: the
      bench probe (`:349`, its sequence `:356`); § L4 in the real viewer (`:398`, the probe →
      marker → sample sequence `:404`, how to run it `:417`, mining `rc.l4` in `agent_logs`
      `:439`); § L4 on a release build: `roomlerd probe-target` (`:460`, its sequence `:469`,
      how to run it `:488`); the index row `docs/README.md:173` links the L0 section.
- [x] **Docs (L1):** `docs/testing.md` gains the L1 encoder bench: how to run it on a host and
      through `roomler exec`, what each cell opens, the table's columns, the verdicts and what
      `fallback` means, how it paces frames and holds its clock, with a mermaid diagram, linked
      from `docs/README.md`. Evidence: § The L1 encoder bench (`docs/testing.md:505`, its
      mermaid `:513`) and the index row `docs/README.md:173`.

## Out of scope

- Choosing the operating point automatically from the profiles (that is the follow-on
  once L1/L2 exist — today codec selection is negotiated by support, not by measured
  cost).
- Conference/mediasoup quality; this is the remote-desktop path only.

## Field log

| date | build | result |
|---|---|---|
| 2026-08-27 | 0.4.9 | Baseline field session above; FR filed. |
| 2026-10-13 | — | Amended: L0 and L4 first, as the instrument of the StarDesk teardown program. Nothing built yet. |
| 2026-10-13 | L0, unoptimised build, neo16 (Windows) | **First run, green:** `clean` 30.2 fps decoded, p50 10.2 ms, p95 14.8 ms, 0 freezes · `delay100` p50 122.3 ms (+112) · `gate50` p50 63.3 ms (+53); the hold also halves the frame rate to 15.1, because it sits in the capture loop · `lossy` 25.2 fps, p50 46.6 ms, p95 62.1 ms, **3 freezes, 7 IDRs** against clean's 4. So loss is visible as freezes and keyframes even with NACK repair on. |
| 2026-10-13 | L0, the same build | **Negative control, red:** `gate50` with its hold set to 0 failed `gate50: p50 rose by 0.4 ms, want ≥ 20` (exit 101); restored. A repeat of `clean` read p50 10.2 ms again. |
| 2026-10-13 | L0, CI runner (Linux), merged head `82da2e5e4` | **Green in CI** (#2097, merged `67d3ca9e8`): `clean` 30.2 fps, p50 12.7 ms, p95 15.5 ms · `delay100` p50 113.9 ms (+101) · `gate50` p50 64.6 ms (+52), 15.3 fps · `lossy` 26.3 fps, p50 38.4 ms, 1 freeze, 7 IDRs. The `clean` fps floor started at 0.9× and is now 0.75×, after a local run under a parallel repo-wide grep decoded 20 fps; a gate that flakes under load gets ignored. |
| 2026-10-13 | L4 in process, unoptimised, neo16 (Windows) | **First probe run, green:** `probe` 64 round trips, p50 48.1 ms · `probe50` p50 171.1 ms (+123). That is more than the 100 ms two directions should cost. The first suspect was the probe period: 100 ms is exactly three frames, so every probe landed on the same phase of the capture clock. The probes now go out at a jittered interval. **Wrong turn:** with the jitter, `probe` read 43 round trips, p50 45.6 ms, and `probe50` p50 169.4 ms (+124), so the phase was not the extra. Windows timer overshoot in the vnet router is the likely cause: L0's `delay100` reads +112 on this box and +101 on the Linux runner. The jitter stays, because the phase-lock is real. |
| 2026-10-13 | L4 in process, the same build | **Negative control, red:** with a probe dropped in the input handler before it reaches the source (one marked line), the test failed `probe: 0 round trips measured, want ≥ 20` (exit 101), with every L0 cell unaffected. Restored from the commit; the diff was that one line. |
| 2026-10-13 | L4 viewer half, headless Chromium 151 (Playwright), the real TS modules bundled | **The browser's reader on frames Chromium decoded itself.** Two markers painted on a canvas: the frame's own number bottom-left, a probe bottom-right. Each case went through `readProbe`, the worker sampler's real `copyTo({rect})` path. A canvas-backed BGRA frame read correctly. Through Chromium's WebCodecs encode → decode: VP9 640×360 at 500 kbps 12/12 I420 frames, VP9 at 150 kbps 12/12, VP9 1280×720 scaled 2× to 640×360 12/12 (the k=2 path), H.264 at 500 kbps 12/12. A canvas played through `<video>` and read in `requestVideoFrameCallback` by `readVideoProbe`: 20/20. Every copy was the 144×48 corner, never the whole picture. |
| 2026-10-13 | the same harness | **Negative control:** the same frames with no probe painted read `null` in every case (0 of 69 frames), although each still carried its own number bottom-left. So the reader can fail, and it does not invent a number. Not yet run: a session against a bench agent. |
| 2026-10-13 | `probe-target`, a debug build, the Windows dev box | **The window, on a real desktop.** `roomlerd probe-target --seconds 4` placed itself at `144x80 at (2416,1520) on a 2560x1600 primary monitor`, flush in the corner in physical pixels on a 150 %-scaled display. A DPI-aware screenshot of the corner showed it above the taskbar: the 16 alternating pad cells, and the plain mid-grey marker area of the "no move yet" state. It closed on its own timer, leaving no process. Its unit tests pin the layout, the cell under each point, the cell centres (the viewer pins the same vectors and checks the agent's mapping lands on each exact pixel), and a painted cell reading back from a frame's bottom-right through the decoder, with nothing read before the first move. Not yet run: a pointer move into the pad. That would move the pointer of the person using this box, so it waits for a bench host. |
| 2026-10-13 | the same | **Negative control, red:** a window that reads its cells 10 px wide instead of 9 (one marked line in `cell_at`) failed `every_cell_centre_lands_in_its_own_cell` (cell 6's centre read back as 5) and `a_point_maps_to_its_cell_and_the_marker_area_to_none`. A drift between where the viewer aims and where the window reads is caught. Restored from the commit; the diff was that one line. |
| 2026-10-13 | **Release build, field.** Agent **0.4.134** (MSI, SystemContext) on a throwaway vmtest Win11 guest (KVM on a fleet host, 1280×800), the **production viewer** (`hosted-20261013-b180d92`) in headless Chromium on a LAN neighbour | **L4 end to end on a release build, first field run, a smoke test rather than an AC cell.** `roomlerd probe-target --seconds 1500` ran in the signed-in session as a scheduled `/IT` task: `144x80 at (1136,720) on a 1280x800 primary monitor`. The viewer set `roomler-rc-bench=target` and was given `data-channel-vp9-444` (libvpx 4:4:4, software). ICE moved relay → direct 5 s in. **15 batches × 40 probes = 600, lost 0. p50 42–49 ms, p95 59–74 ms, max 62–230 ms**, with two batches above 200 ms. Every round trip is a real `mouse_move`: SendInput in the guest, its compositor, DXGI capture, VP9 software encode, the data channel, WebCodecs decode in the worker sampler. The viewer's display is not in it. The streamed corner showed the window's pad and marker in the viewer's screenshot. Not an AC cell: a VM, a software encoder, no real display. The direct and relay cells on real hosts are still owed. Same run: the harness's own `win11/installer/system` cell passed 17 of 18. The miss is `probe-error`, its FR-84 companion-UI probes (`page.click: Timeout 30000ms exceeded`), outside this FR |
| 2026-10-16 | **Release build, field: L4 as FR-98 P5's instrument.** Agent **0.4.138** (MSI, service) on a throwaway vmtest Win11 guest, the production viewer in headless Chromium on a LAN neighbour, `roomlerd probe-target` in the signed-in session; the viewer forced onto the RTP track or left to its own DataChannel pick | **L4 told three paths apart on one guest.** The DataChannel (`data-channel-vp9-444`): batch p50s 44–52 ms, 0 of 480 lost, as on 10-13. The RTP track on OpenH264: 35–40 ms with FR-98's `playout-delay` on, 78–362 ms with it off. The RTP track on Auto: **~850 ms with a quarter of the probes unanswered in 2 s**, because Auto had picked Microsoft's software MFT, which no other instrument here could see (#2283). FR-98 §9 has the full rows |
| 2026-10-30 | **L1, a release build of the branch** (#3155) with the Linux release lane's features (`release-agent.yml:552`), in an Ubuntu 24.04 container on the Windows dev box, no GPU, no config | **RED:** the released 0.4.188 `.deb` (sha256 checked) answers `encoder-smoke --bench` with `error: unexpected argument '--bench' found`, exit 2, and no `L1` line. Its plain smoke still passes (`openh264`, 4 keyframes, 13 238 bytes). **GREEN:** the quick profile printed six rows in 2.0 s, 18.5 KB with its log lines against exec's 256 KiB. `rtp:h264` is OpenH264 `sw-only` (p50 6.6 ms, p95 12.9 ms, 3383 kbps for a 5529 kbps target). `rtp:h265` and the three FFmpeg DataChannel cells read `none`, as no GPU means they should. **`dc:vp9:444` reads `fallback`**: the build carries FFmpeg's VP9 hardware path, the container has no device for it, and libvpx answered (p50 5.2 ms). The release lane's own line on this binary gave 0.4.188's result exactly. The thread's clock reads `refused:95`: this kernel has no util clamp. **Twice:** `rtp:h264` over static, drag and pan, back to back, repeated its sizes and rates to the byte. Its p50s were 3.7/4.7, 8.1/7.5 and 7.6/6.4 ms, and its p95s up to 60 % apart (27.8/17.2 ms on pan) on a busy host. A paced run read about the same as back to back on this x86 host. **Wrong turn, fixed:** the first GREEN read `rtp:h264` `idr_n=6` with an IDR p50 of 30 bytes. OpenH264 returns an IDR as two packets (the parameter sets, then the slice), and the bench counted packets. A frame is now one `encode()`'s output, and against the old count the seam test fails `6` against `3` |
| 2026-10-31 | **L1, the fleet.** Agent 0.4.191 on every host read, the quick profile through `roomler exec` | **The check fails where it should.** Both Windows hosts read `fallback` on `rtp:h264`, on a hello that lists `mf-h264-hw`; jupiter and mars matched their hellos. 4 of 11 online hosts reached, with a live-use check inside each command. The table, the join and who was skipped are in [§ L1 fleet run](#l1-fleet-run-2026-10-31). |

### L1 fleet run, 2026-10-31

Agent 0.4.191, the quick profile (`roomlerd encoder-smoke --bench`: 1280×720, `drag:1000`, 90
frames back to back at a 30 fps target, hardware asked for). One host at a time, one run each.
An entry is what opened, its verdict, then encode p50 / p95 in ms and the Mbps it produced. The
Linux runs were `nice -n 10`, and their `clock=` reads `clamped`. The Windows runs read `n/a`.

| | jupiter | mars | A Windows laptop | A Windows desktop |
|---|---|---|---|---|
| Host | Linux 6.8, AMD Raphael iGPU, load 0.5 on 32 CPUs | Linux 6.8, no GPU, load 4 on 16 CPUs | Core Ultra 7 355, Intel iGPU, on AC | Core i5-12400, UHD 730 |
| `rtp:h264` | `openh264` sw, `sw-only`: 5.67 / 10.71, 3.4 | `openh264` sw, `sw-only`: 13.21 / 25.61, 3.4 | **`mf-h264` sw, `fallback`**: 2.26 / 2.73, 3.1 | **`mf-h264` sw, `fallback`**: 2.59 / 3.36, 2.6 |
| `rtp:h265` | `hevc_vaapi` hw, `ok`: 2.73 / 3.10, 4.5 | `noop`, `none` | `hevc_qsv` hw, `ok`: 5.59 / 6.29, 6.3 | `hevc_qsv` hw, `ok`: 2.66 / 3.36, 6.3 |
| `rtp:av1` | not in this build | not in this build | `noop`, `none` | `noop`, `none` |
| `dc:h264` | `h264_vaapi` hw, `ok`: 2.63 / 2.97, 1.2 | nothing, `none` | `h264_qsv` hw, `ok`: 6.41 / 7.26, 5.1 | `h264_qsv` hw, `ok`: 2.78 / 3.50, 5.1 |
| `dc:h265` | `hevc_vaapi` hw, `ok`: 2.67 / 3.14, 4.4 | nothing, `none` | `hevc_qsv` hw, `ok`: 7.08 / 8.20, 5.1 | `hevc_qsv` hw, `ok`: 2.74 / 3.58, 5.1 |
| `dc:av1` | nothing, `none` | nothing, `none` | `av1_qsv` hw, `ok`: 6.94 / 8.02, 4.6 | nothing, `none` |
| `dc:vp9:444` | `libvpx-vp9-444` sw, `fallback`: 3.77 / 5.52, 3.6 | `libvpx-vp9-444` sw, `fallback`: 9.58 / 14.30, 3.6 | `libvpx-vp9-444` sw, `fallback`: 8.05 / 10.89, 3.6 | `libvpx-vp9-444` sw, `fallback`: 4.80 / 8.74, 3.6 |
| Whole run | 3.4 s | 5.4 s | 10.7 s | 5.3 s |
| Hello's `hw_encoders`, hardware entries | `hevc_vaapi`, `h264_vaapi` | none | `mf-h264-hw`, `vp9_qsv`, `hevc_qsv`, `av1_qsv`, `h264_qsv` | `mf-h264-hw`, `vp9_qsv`, `hevc_qsv`, `h264_qsv` |
| Hello's `h264/mf` cell | no MF on Linux | no MF on Linux | software | software |

- **The fallback AC1 asked for.** Both Windows hosts list `mf-h264-hw`, and `rtp:h264` opened
  Microsoft's software MFT. The log says why: the cascade enumerated 4 MFTs and deferred the
  asynchronous ones, the last produced zero bytes, and it then wrote `no HW candidate succeeded —
  falling back to SW MFT`. This is the host, not how exec runs: the same process opened `hevc_qsv`
  and `h264_qsv` in hardware on its next rows.
- **The hello half knows.** On both hosts the same hello's FR-77 cell marks `h264/mf` as
  software. Only the legacy label overclaims, because it is enumeration-only
  (`agents/roomlerd/src/encode/caps.rs:1883`). A reader of `video_cells` is not misled.
- **The join can pass too.** jupiter's two VAAPI encoders opened as its hello says, and mars
  advertises no hardware and opened none.
- **`dc:vp9:444`'s `fallback` is a different thing.** It says this build has a VP9 hardware
  path and software answered. No hello claims 4:4:4 VP9 in hardware: the Windows hosts' `vp9/qsv`
  cell is 4:2:0.
- **Read mars's times with care.** The same frames took 5.67 ms on jupiter and 13.21 ms on mars,
  at load 4 and a lower priority. The bytes do not depend on the host: `rtp:h264` produced 3383
  kbps on both and in the 2026-10-30 container, and `dc:vp9:444` 3594 kbps on all four.
- **A cell the hello advertises and a session may not open.** The laptop's hello lists `av1` in
  `codecs`, yet `rtp:av1` opened `noop`. FFmpeg's AV1 probe adds the codec
  (`agents/roomlerd/src/encode/caps.rs:2208`), the RTP path tries only Media Foundation
  (`agents/roomlerd/src/encode/mod.rs:1642`), and `pick_best_codec` ranks AV1 first with no
  per-path filter (`agents/roomlerd/src/signaling.rs:3492`,
  `agents/roomlerd/src/encode/caps.rs:2949`). The code logs `Session will have no video until
  reconnect` there. `agent_logs` holds that line 14 times in the org in 7 days, all on
  CORPLAP-3, whose hello has the same shape, the last on 2026-10-30. None of those batches
  carries a session id, so this shows the path fires, not that a viewer lost its picture. Not
  fixed here.

Who ran it, against the server's 20 rows, 11 of them online:

| Hosts | State | Why |
|---|---|---|
| jupiter, mars, a Windows laptop, a Windows desktop | reached | mars on the second try: the first stopped at the check, at load 18 on 16 CPUs |
| zeus | skipped | Host under memory pressure after an incident the same evening. Its hello matches jupiter's |
| MacBook-1 | skipped | Two VM guests of another job were running. FR-112's rows on [#2430](https://github.com/gjovanov/roomler-ai/issues/2430) ran `--encoder software`, which cannot read `fallback`, so its VideoToolbox rows are owed |
| CORPLAP-1, CORPLAP-2, CORPLAP-3 | not run | `roomler exec` is not used on a corporate laptop |
| NEO16, NEO16-WSL | not run | Never a target |
| 9 other rows | offline | |
