# FR-16: Systematic remote-desktop quality benchmark

Status: **proposed** (2026-08-27). Tracking issue: `FR-16` (#798). Sibling of FR-17
(#799, the architectural fix); the measurement half of the FR-1 program.

## Goal

Turn "remote desktop quality" from a set of anecdotes into a **repeatable, scored
matrix** over the dimensions that actually vary: codec (AV1 / H.265 / H.264 / VP9 /
VP8), chroma (4:2:0 / 4:4:4), encode and decode path (HW / SW), transport (direct /
relay), resolution and fps, and the device pair. Without it, every optimisation is
argued from one person's impression of one drag.

## Why the current method cannot answer the question

Field session 2026-08-27, six machines on ONE desk sharing one Wi-Fi (neo16, CLK[vpn],
pc50045[vpn], pc55331, MacBook, ROZALINA), operator driving each pair by hand:

| pair | result |
|---|---|
| neo16 → CLK | fine |
| pc50045 → CLK | 60 ms baseline, 600+ ms spikes |
| pc50045 → ROZALINA | 1–2 s frozen at drag start, then ~80 ms, "not smooth" |
| MacBook → pc50045 (H.265) | **20 000+ ms**; VP9 HW better but still sluggish |

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

## Acceptance criteria

- [ ] L1 runs on every fleet host through `roomler exec` and produces a per-host
      encoder cost table, including a host where an advertised HW encoder actually
      falls back (proving the check can fail).
- [ ] L2 produces an N×N pair matrix with carrier + RTT + goodput.
- [ ] L3 reproduces a known-bad cell (MacBook → pc50045 H.265) and a known-good cell
      (a direct pair) with scores on opposite ends of the ladder — the harness must
      first REPRODUCE the field results, or it is measuring something else.
- [ ] Two consecutive runs of the same cell agree within a stated tolerance
      (repeatability, without which nothing can be A/B'd).
- [ ] The FR-17 before/after is reported as a distribution from this harness.

## Out of scope

- Choosing the operating point automatically from the profiles (that is the follow-on
  once L1/L2 exist — today codec selection is negotiated by support, not by measured
  cost).
- Conference/mediasoup quality; this is the remote-desktop path only.

## Field log

| date | build | result |
|---|---|---|
| 2026-08-27 | 0.4.9 | Baseline field session above; FR filed. |
