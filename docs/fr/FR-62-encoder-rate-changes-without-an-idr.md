# FR-62 — Encoder rate changes without an IDR

**Issue:** [#1242](https://github.com/gjovanov/roomler-ai/issues/1242) · **Status:** **complete 2026-11-05**: AC1–AC3, AC5 and AC6 met; AC4 moved to FR-63 (its B4 and AC7) · A0 measured on BOTH silicon — NVENC passes, **QSV in-place is broken and the flag stays OFF**; **both** recorded causes are REFUTED (the second by measurement on 0.4.62); patch 0004 is what is left; A1 shipped inert; A2 + const flip shipped (0.4.51) · **A3 — AMF measured** on the Radeon 610M (0.4.187, 2026-10-29): a rebuild on every rung, **41–55 ms, 20/20 IDRs** — the 0.4.187 harness filed them as periodic, fixed in the same PR · **VideoToolbox measured 2026-11-05** (Apple M5 Pro, 0.4.201): a rebuild on every rung, 8–12 ms, 20/20 IDRs · NVENC re-read on the released 0.4.187: **0/20** · **AC2/AC3 met on 0.4.201** (2026-11-05): a relayed `av1_nvenc` session under a window drag made 13 in-place rate moves, none with an IDR of its own, and each saturated window after a move landed at 0.97–1.00 of the new rung; the three unexplained IDRs of the 2026-11-03 passive read did not recur · docs: [`encoders.md` § A rate move, per backend](../encoders.md#a-rate-move-per-backend-fr-62) · the kill-switch registry (AC4's instrument): [`rate-control.md` § Kill switches](../rate-control.md#kill-switches-and-when-each-retires--the-registry-fr-62-ac4) · **Plan:** `rate-control-architecture` (approved 2026-09-02)

## Progress

- **2026-11-05 19:35Z — A4 and AC4 move to FR-63, and FR-62 is complete.** The operator's call,
  on this case:
  - The plan always sequenced A4's deletions after FR-63's B2 flip plus a release soak
    (Sequencing row 5), and FR-63's B3 deletes `aimd.rs`, the coarsen ladder with it, after the
    same flip.
  - The measurements bound A4 to NVENC. QSV's A0 failed, and VideoToolbox, AMF, D3D12 and Vulkan
    rebuild with an IDR on every move, so the rationing heuristics stay load-bearing everywhere
    but NVENC.
  - So FR-63 carries it, as phase B4 and AC7: retire the IDR-rationing heuristics through the
    registry table, per backend, NVENC only while every other backend rebuilds with an IDR. The
    registry (`rate-control.md` § Kill switches) and its A4 rows stay as they are.
  - What FR-62 set out to prove holds. An NVENC rate move costs no IDR, on silicon (A0, 0/20) and
    in the field on a relayed session (13 moves, none with an IDR), and every backend has its A0
    row.
- **2026-11-05 19:21Z — A3: VideoToolbox measured, a rebuild with an IDR on every rung. AC2's
  wording now matches its read.**
  - **VideoToolbox.** MacBook-1 (Apple M5 Pro, macOS 27.0.1) on the released 0.4.201, one sweep:
    `encoder-smoke --reconfigure-sweep --name hevc_videotoolbox --codec hevc --json`, run as root
    over `roomler exec`, with no lab VM running. `hevc_videotoolbox` rebuilt on all 20 moves
    (`total_rate_idr: 20`, `total_periodic_idr: 0`): 8.3–12.1 ms each, median 11.5 ms. Each
    rung's first frames peaked at 1.3–3.2 KB, against 0.3–0.7 KB after them. `pass: false` on
    `idr+apply`.
  - So VideoToolbox has AMF's shape at about a quarter of AMF's cost. The rebuild's time is no
    argument for patch 0002; its IDR is. Until someone writes that patch, VideoToolbox keeps the
    rebuild path, and the coarsen ladder rations its IDRs. A4 row 2's precondition ("VideoToolbox
    is measured") now holds, and the reading keeps the ladder.
  - The sweep's log names every rebuild "QSV/AMF encoder rebuilt", a message older than the
    VideoToolbox arm.
  - **AC2's wording**, on the operator's go: it now says what the corrected read judges, the first
    complete window after each rate move under motion that saturates the cap. It read "≈ 1.0
    within 2 s of every target change", a CBR's contract.
- **2026-11-05 18:53Z — the motion read: 13 in-place rate moves on a relayed NVENC session, and
  not one IDR of their own. AC2 and AC3 are met.**
  CORPLAP-2 on 0.4.201, `av1_nvenc` 1920×1080, `constrained=true` on all 217 heartbeats over
  439 s. The viewer's stored display match set the laptop to 1920×1080 at the open and restored
  it at the close. As on 2026-11-03, the viewer's pair (host↔prflx) rode the overlay and DERP
  carried the overlay: the agent's carrier check read `Relay`, and the agent recorded the
  session's carrier as `derp`. The operator typed the OK into the reading session, naming the
  sequence, and it ran once: the Sticky Notes title-bar drag there and back for 220 s (4,104
  pointer moves, released where it began), one `rc:keyframe`, one `rc:resolution` to 1600×1000
  and back. No key, no other click, no config change.
  - **The moves.** During the drag the FR-35 learner stepped the ceiling, and the target with it,
    from 3.0 to 3.39 M, inside the 3 M rung, so nothing moved. When the DERP carrier congested at
    18:58, the target fell to 2.45 M (the first rung crossing) and then, as FR-59 P6 dropped the
    learned ceiling for the measured pipe, to 0.51 M. From there it climbed back. `rate_moves`
    went 0 → 13: down to the 550 k rung, up through 750 k, 1 M and 1.5 M, then down and up
    between 1 M and 3 M after the drag. `rebuilds` stayed 0, and `reconfig_forces_idr` read
    `false` throughout.
  - **AC3: every IDR has its source.** `idr_count` went 1 → 5 from the first heartbeat (the
    opener of the encoder that the display match re-opened) to the close:

    | Window ending | Δ`idr_count` | Beside it |
    |---|---|---|
    | 18:58:49 | +1 | our `rc:keyframe` at 18:58:48.3, the positive control; a move 1.5 M → 1 M shares the window and adds nothing |
    | 18:59:01 | +1 | the resize to 1600×900, `dims_swaps` +1, the positive control |
    | 18:59:15 | +1 | the resize back, `dims_swaps` +1 |
    | 18:59:28 | +1 | the viewer's own `rc:keyframe` at 18:59:26.6, seen by the viewer-side tap; the agent logs the request at `debug` (`peer.rs:11469`) |

    The other 12 moves sit in windows where `idr_count` did not move: 216 windows judged, none
    unexplained. The inequality holds as written: 4 ≤ 2 dims changes + 2 keyframe requests.
  - **AC2: ≈ 1.0 where the drag saturated the cap.** The first complete window after each move,
    Δ`bytes_written` ÷ Δ`frames_encoded` against the new rung ÷ `target_fps` ÷ 8: 0.967
    (→ 550 k), 1.001 (→ 750 k), 0.992 (→ 1 M) and 0.978 (→ 1.5 M, 10 s after the release, the
    bytes still at the cap). After the drag, on a still screen, the windows read 0.17–0.71: a
    quality-capped VBR spends only what the picture needs. The highest window against its cap
    read 1.228, at 18:58:01, still under the opener's 2.55 M before the first rung crossing.
  - The wire carried 6 keyframes in 10,914 frames: two at the open (the first encoder's, then
    the re-opened one's), ours, the two resizes and the viewer's own.
  - 🔑 **AV1 on NVENC moves in place as HEVC does.** `resolve_rate_mode` sends every name with
    `nvenc` in it to `InPlaceVbr` (`encode/ffmpeg/encoder.rs:869`), and patch 0001 edits
    `nvenc.c`'s `reconfig_encoder`, which all three NVENC codecs share. The A0 sweep measured
    `hevc_nvenc`; this is the first read of `av1_nvenc` moving.
  - 🔑 **The three 2026-11-03 IDRs did not recur.** The overlay churned as it did then: forced
    rekeys unanswered over DERP at 18:57:02 and 18:58:32, a one-way carrier at 18:59:22, a
    make-before-break probe that kept the relay at 18:59:27. The one IDR near that churn was the
    viewer's own request. The question stays open.
  - The permission check refused the first Connect click as a write to the remote host, because
    the OK typed first said only "go". A second OK, typed in the session with the sequence
    spelled out, cleared it.
- **2026-11-03 22:23Z — the second relayed read: no rate move again, and three IDRs that nothing
  the read counted explains. The carrier line logged beside them is not their cause.**
  CORPLAP-2 on 0.4.199, `hevc_nvenc` 1920×1200, `constrained=true` on all 57 heartbeats over
  113 s. The viewer's pair was host↔srflx over the overlay, and the overlay link under it was
  DERP-relayed (`relay:derp/tcp`): relayed by DERP this time, not by TURN. One session from a
  viewer tab, no config change.
  - **The motion read did not run.** The operator had approved one exact sequence: a Sticky
    Notes title-bar drag there and back for 220 s, one keyframe request, one resize to
    1600×1000 and back. The approval reached the worker through another session, and the
    automation's permission check refused the drag twice as a write to the remote host. The
    sequence stopped under the operator's own rule (retry once, then stop). No input reached
    the laptop.
  - **`rate_moves` stayed 0, so AC2 and AC3 are still not proven.** `swaps`, `rebuilds`,
    `dims_swaps` and `frames_dropped_backpressure` stayed 0 as well. The rate memory seeded the
    opener at 4,046,610, and the encoder opened at 3,439,618.
  - **`idr_count` read 14 at the first heartbeat** (the opener plus `dc_unopen_drops=13`) and 17
    at the last. The wire carried 4 keyframes in 3,544 frames: the opener, then one each at
    22:24:37, 22:25:28 and 22:25:44.
  - 🔑 **Nothing the read counted explains those three.** Beside each one: no rate move, no
    `rc:keyframe` from the viewer, no drop, no dims swap, no swap and no rebuild. None of them
    sits beside a rate move, so none is AC3's RED. But AC3's read has to name every IDR before
    it can call one rate-caused.
  - 🔑 **The carrier line is not a source.** Each of the three came 1–3 s after an agent line
    `overlay carrier under the nominated pair is not direct`. The code read here is master's,
    and none of these paths has changed since 0.4.199, the build that ran. The line comes from
    `overlay_remote_is_relay_tier` (`peer.rs:2291`), a LocalAPI read that changes nothing. The
    FFmpeg DC pump calls it on its transport recheck, every 5 s (`TRANSPORT_RECHECK_INTERVAL`,
    `peer.rs:2176`; the recheck at `:7802`). The answer goes two places:
    - into `constrained`, which sets the rate ceiling and, on a flip, re-sends `rc:video-info`
      (`:7672`);
    - into `FlipTracker` (`encode/rate_profile.rs:81`). It rebuilds the encoder only after two
      consecutive checks disagree with the state the pump was built for, at least 60 s after
      the last rebuild, and only when the frame rate changes. That rebuild logs its own line
      (`peer.rs:7834`), re-opens the encoder and restarts the counters. Of the four `send_epoch`
      bumps, only the inline open's (`:9010`) can follow from this path, through that rebuild.
      The others are a dims swap (`:7506`), a rate swap (`:7584`) and a deferred rebuild
      (`:9571`).

    Neither happened: every heartbeat read `constrained=true`, and `idr_count` never restarted.
    The 1–3 s is the line's cadence. The heartbeat's `constrained` is the variable the recheck
    writes, and a recheck that answered `false` would have shown on the next two heartbeats.
    So 57 of 57 `true` means every recheck took the branch that prints the line: it printed
    every 5 s for the whole session, and every event in the session fell 0–5 s after one. The
    WireGuard rekey timeouts and the failed make-before-break probe beside it belong to the
    overlay. The pump sees the overlay only through the carrier this recheck asks for.
  - 🔑 **On this cell the encoder never picks an IDR by itself, so each of the three was
    forced.** `idr_count` counts every key-flagged packet (`encode/ffmpeg/encoder.rs:2392`).
    NVENC opens with a 64,800-frame GOP (`:51`, set at `:1905`), over half an hour at this
    session's 31 fps, and with `rc-lookahead=0` and `bf=0` (`:366`–`:367`). Its scene-cut
    I-frames need the lookahead, so they are off. A key frame therefore comes from a force
    (`request_keyframe`, `:3002`) or from frame 0 of an encoder the pump opened or adopted
    (`:2355`). Every place the FFmpeg DC pump forces one is below, with what shows it. This
    replaces the 2026-10-31 table further down, which lacked the PLI path, a second viewer and
    the rebuilds:

    | Source | Site (`peer.rs`) | What shows it |
    |---|---|---|
    | the opener, and one force per frame while the video DC is not open | `:9800`, `:9806` | `dc_unopen_drops` |
    | frame 0 of an adopted dims swap or rate swap | `:7477`, `:7581` | `dims_swaps`, `swaps` |
    | frame 0 of an inline open: a dims change, the force-ignored fallback (`:9158`) or a persisted transport flip (`:7679`) | `:9051` | the counters restart at zero; the fallback warns (`:9165`), the flip logs `:7693` |
    | the viewer's `rc:keyframe`: a decode backlog, a rebuilt decoder, or the stall probe after 6 s with no media | `:11164` | the viewer's send on its control channel, and `keyframe_requests` on `rc:session.stats`. The agent's own line is `debug!` (`:11166`) |
    | a PLI or FIR on the session's RTP video track | `:843` | `PLI/FIR → forcing keyframe` |
    | a lock or unlock | `:9108` | `lock_state: transition observed` (`lock_state.rs:471`) |
    | the idle-settle keyframe | `:8293` | `idle-settle keyframe (motion burst ended)`. On a constrained session it is suppressed while `relay_idr_thrift` is on (the default), and counted in `settle_kf_suppressed` |
    | a second viewer joining, or its video DC going live | `:9122` | `media_share: joined shared encoder pipeline as follower` (`media_share.rs:860`), then `…re-syncing onto a fresh IDR` (`:942`) |
    | a second viewer's own `rc:keyframe`, or its queue overflowing | `:9122` | nothing on the reading tab. Its join logs |
    | a force left unanswered for 4 s | `:9134` | `keyframe force retry engaged`, once per pump |
    | a drop-burst resync: the send channel filled, so the first frame through forces the next | `:9877`, then `:9868` | `frames_dropped_backpressure` |

    `invalidation_requested` is read only by the RTP pump (`:3376`), and no keyframe request
    travels over the signalling WS.
  - **What is left.** The three stay open (Open decisions). The read's ledger was the heartbeat
    counters and the reading tab's own sends. The log lines in the table were not part of it,
    and a second viewer's requests never show on the reading tab. The next read searches the
    agent log for every line in the table right after the session ends, because the log
    listing returns only a device's newest 500 batches
    (`crates/modules/fleet/src/agent_log.rs:226`).

- **2026-10-31 22:47Z — the first relayed NVENC read: the cell works, and nothing was tested.**
  CORPLAP-2 on 0.4.191, `hevc_nvenc` 1920×1200, `constrained=true` on all 158 heartbeats over
  318 s, the viewer's pair local `srflx`, remote `relay`, UDP. One session from a viewer tab, no
  config change, read through the agent-log API.
  - **`rate_moves` stayed 0, so AC2 and AC3 are not proven.** Four target changes (3.13–3.68 M)
    all stayed on one rung. The 3 M rung spans 2.5–3.75 M, and on a held screen nothing drives
    the target out of it. By the arithmetic the Priority dial's 70 % lands on the same rung at
    this ceiling, and the FFmpeg DC pump does not read `rc:quality`. Rate moves on this cell
    need real desktop motion.
  - **`idr_count` read 18 at the first heartbeat:** the opener plus 17 forced while the DC was not
    open (`dc_unopen_drops=17`). It stayed 18 for the rest of the session, and the wire carried 1
    keyframe in 6,006 frames. So the read's baseline is load-bearing: without it, `idr_count`
    breaks AC3's inequality before the first picture arrives.
  - **Bytes per frame ran at p50 0.063, max 0.491, of the cap.** An idle desktop, no overshoot.
  - 🔑 **The encoder's maxrate is not always the rung.** It held the 3,677,739 it opened with:
    `set_bitrate` compares the two values' rungs and returns when they match
    (`encode/ffmpeg/encoder.rs:3018`), so the opening value stands until the first rung crossing.
    The read's denominator follows it.
  - **Not run:** the positive controls and the motion. A scripted timeline (one keyframe request,
    one resize and back, a window drag, dial moves) was refused by the automation's permission
    check as a write to the remote host, so the session stayed passive apart from one pointer
    move with no press. They need the operator (Open decisions). VideoToolbox's A0 did not run
    either: lab VMs were live on the Mac.

- **2026-10-31 — AC2/AC3's read corrected, AC4's registry built, AC4's sequencing recorded.**
  - 🔑 **On NVENC the default build already moves the rate in place, with no IDR.**
    `resolve_rate_mode` (`agents/roomlerd/src/encode/ffmpeg/encoder.rs:869`) returns
    `InPlaceVbr` for every `*_nvenc` whatever `encoder_inplace_rate` says, and
    `NVENC_RECONFIG_FORCES_IDR` (`:893`) has been `false` since patch 0001 shipped
    (0.4.51). On NVENC the flag only sizes the VBV window that a move writes
    (`inplace_rate_bufsize`, `encode/mod.rs:363`). So a constrained NVENC session needs
    no config change to be read for AC2/AC3, and flipping the flag off does not produce
    a RED (no IDR per rate step). The read's RED is a **positive control**: a viewer
    resize that moves `dims_swaps` by one must move `idr_count` by exactly one. That
    holds because a dims swap adopts through `adopt_rebuilt` (`peer.rs:7618`), which
    keeps the encoder's counters. A read that cannot see that IDR cannot vouch for the
    absence of the others.
  - 🔑 **AC3's read, as written, could fail with no rate-caused IDR at all.** Its
    inequality names two sources, and its read counted viewer requests from
    `PLI/FIR → forcing keyframe` lines, which only the RTP path prints (`peer.rs:850`).
    Every NVENC HEVC or AV1 session is on the DataChannel, where a viewer asks with
    `rc:keyframe`. The FFmpeg DC pump has these sources (the 2026-11-03 table above is the
    complete list):

    | IDR source | Site (`peer.rs`) | What shows it |
    |---|---|---|
    | the opener, and one force per frame while the DC is not open yet | `:9800`, `:9806` | `dc_unopen_drops`. Baseline the read after the first heartbeat |
    | a dims swap | `:7505` | `dims_swaps` |
    | the viewer's `rc:keyframe`, sent when its decode queue backs up | `:11165` | `keyframe_requests` on `rc:session.stats`, every 30 s. Its log line is `debug!` |
    | a drop-burst resync: the send channel filled, so the next enqueue forces an IDR | `:9877`, then `:9868` | nothing of its own. `frames_dropped_backpressure` shows the burst |
    | a lock or unlock | `:9108` | the session's lock state |
    | a settle keyframe | `:8293` | suppressed on a constrained session (`settle_kf_suppressed`) |
    | a force unanswered for 4 s | `:9134` | one log line per episode |

    The drop-burst resync is the one that matters: it is what a congested relay does,
    and it is neither a dims change nor a viewer request. So the read is per window, on
    the heartbeat's cumulative counters: **every Δ`idr_count` must have a Δ`dims_swaps`, a
    Δ`frames_dropped_backpressure`, a `keyframe_requests` tick or a lock edge beside it.
    An unexplained IDR in a window with Δ`rate_moves` > 0 is the RED.** The session must
    also show real moves, or the read proves nothing. An inline re-open (`peer.rs:9204`)
    builds a new encoder whose counters restart at zero, so a window across one is not
    diffed.
  - 🔑 **AC2's read had the wrong denominator and the wrong trigger.**
    - `target_bps` is `governor.applied_bps()`, the raw target. The encoder runs at the maxrate
      it opened with until the first rung crossing, then at that target's nearest rung
      (`coarsen_bitrate`, `encode/aimd.rs:317`, applied in `set_bitrate`, `encoder.rs:3017`).
      So the read is against that maxrate ÷ `target_fps` ÷ 8. Against the raw target a
      perfect encoder reads off by the rung's distance.
    - `DC pump set_bitrate (AIMD)` prints every target change, and most stay on a rung
      and move nothing. Δ`rate_moves` marks a real move. The read judges the first
      complete 2 s heartbeat window (`peer.rs:7123`) after a Δ`rate_moves`, on
      Δ`bytes_written` ÷ Δ`frames_encoded`. A window that starts at most 2 s after the
      move and holds the ratio shows the encoder had tracked by then.
    - ≈ 1.0 is a CBR's contract. NVENC is quality-capped VBR and sits under its cap on
      compressible content, the same reason the A0 sweep gates only QSV on the ratio. The
      session needs motion that saturates the cap. Outside such windows the read is
      one-sided: no overshoot.
  - **The cell.** Of the org's test hosts, only two have NVENC: the dev box (the
    operator's own machine, never a test target) and CORPLAP-2, a corporate laptop
    whose `av1_nvenc` sessions relay over its VPN without any pin. So AC2/AC3 wait for
    the operator to choose the cell. The other test hosts are QSV laptops, AMD VAAPI
    servers, a VideoToolbox Mac and a server with no GPU.
  - **AC4's instrument exists.** Plan rule 4's table is now
    [`rate-control.md` § Kill switches](../rate-control.md#kill-switches-and-when-each-retires--the-registry-fr-62-ac4).
    It lists 18 switches with the evidence that retires each and the phase that
    deletes it, plus the four A4 heuristics that have no switch of their own, each
    with its plan precondition and its state. Nothing has been deleted.
  - **Why A4 does not start now.** The plan sequences A4's deletions after FR-63's B2
    flip plus one release soak (Sequencing row 5), and every A4 row's precondition is
    unmet: row 1 needs "A0 passes on QSV", which failed; row 2 needs VideoToolbox
    measured, and QSV still needs the ladder; row 3 is marked "a behavioral decision for
    the post-soak pass" in the code (`encode/governor.rs:1017`). Found on the way:
    B3b deletes `aimd.rs` whole, the coarsen ladder included, and the plan predates
    A0-QSV. Whatever replaces the AIMD must carry QSV's rationing forward.

- **2026-10-29 — A3 (AMF), the released-build re-read, and the harness's attribution fix.**
  - `hevc_nvenc` on the released **0.4.187** (RTX 5090 Laptop, the same GPU as the 2026-09-02 read):
    **0/20** rate-caused IDRs, apply 0.004 ms, in both the default and the `--constrained` HRD arm —
    against 20/20 on the unpatched 0.4.50. The patch is in the shipped bytes; the gate holds.
  - `hevc_amf` on the Radeon 610M, driven past the cascade with `--name hevc_amf` (FR-78 P2's
    selector, which the 2026-09-02 "AMF deferred" note was waiting for): **every rung is a rebuild
    (`set_bitrate: QSV/AMF encoder rebuilt`), 41–55 ms each, and every rebuild ships an IDR** —
    `first3_max` 1.4–5.4 KB against a 250–690 B trailing mean on all 20 rungs. AMF is therefore a
    D3D12/Vulkan-class backend (an IDR plus a 40–80 ms open), not a QSV-class one (a multi-second
    stall at slow-link rates). Patch 0003 (`amf-runtime-bitrate`) stays disabled; A4's rationing for
    AMF is about the IDR, not the stall.
  - 🔑 **The harness misfiled AMF's IDRs.** The sweep charged the rate change with a keyframe on
    frame 0 of the rung. AMF (`query_timeout=1`) returns nothing for the frame that fed the rebuilt
    encoder and emits its IDR on the next call, so 0.4.187 reported `total_rate_idr: 0,
    total_periodic_idr: 20` — one "periodic" IDR per rung, which is the rebuild's. The attribution
    now keys on the rung's **first output frame** (`sweep_attribute_idrs`, `app.rs`), unit-tested
    with both shapes; NVENC (output on frame 0) reads as before.
  - 🔑 **The verdict is per backend.** `pass` had been gated on the bytes-per-frame ratio and the
    burst for every backend, which read `false` for NVENC on 0/20 IDRs and a 0.004 ms apply (the
    2026-09-02 caveat, twice noted, never fixed). `sweep_verdict` gates every backend on the
    content-independent pair (no rate-caused IDR, apply < 5 ms) and only QSV — the CBR backend,
    where ratio ≈ 1 is the contract — on the tracking pair; the JSON names its `basis`.
  - AC5's unit half: `inplace_rate_bufsize` (`encode/mod.rs`, ungated) pins the OFF arm's 1× write
    beside the existing `rate_mode_resolves_per_backend_and_flag`; the field half is read below.
  - **Found on the way, recorded, not changed:** the ladder tops out at 25 Mbps. A direct
    `av1_nvenc` session at 144 fps on the dev box (0.4.186, 2026-10-29 10:17Z) climbed `target_bps`
    34 → 65 Mbps in eight AIMD steps while `rate_moves` stayed 0 — every step coarsened to the top
    rung and `set_bitrate` returned. Above the ladder the controller moves nothing. One for FR-63
    and A4, where the ladder's fate is decided.
  - Docs: `encoders.md` § "A rate move, per backend (FR-62)" — the arm per backend, the measured
    cost table, the heartbeat counters, the sweep's rules, the patch gate, the switches with their
    retirement conditions; a `docs/README.md` row.
- **A0 — the sweep** (`encoder-smoke --reconfigure-sweep`) shipped in #1247.
- **A0 — NVENC measured on the RTX 5090** (2026-09-02): `hevc_nvenc` forces a rate-caused IDR on
  **20/20** rungs, but the apply is **0.001–0.008 ms** (an in-place `AVCodecContext` field write).
  So the entire cost of a NVENC rate move is the forced keyframe — exactly what patch
  `0001-nvenc-no-idr` removes. Codec-independent (shared `reconfig_encoder`). Full table on #1242.
- **A1 — the in-place path** ships behind `encoder_inplace_rate` (**default OFF, inert**): QSV wrote
  `bit_rate + rc_max_rate + rc_buffer_size` (CBR) so `qsvenc`'s per-frame `update_bitrate` resets the
  BRC with no rebuild; NVENC's in-place buffer is sized to the open window (fixing the pre-A1 1×
  write). `supports_dynamic_bitrate()` = "not a rebuild" so the pump immediate-applies QSV when on.
  Heartbeat gains `inplace_rate / rate_moves / rebuilds / idr_count`.
  ⚠️ This arm **does not work on Iris Xe** and the flag stays OFF; see "Why the Reset fails" below
  for the two explanations that have been tried and refuted.
- 🚨 **A0-QSV MEASURED, AND IT REFUTED THE PLAN'S ASSUMPTION** (CORPLAP-1's Iris Xe, 0.4.51,
  2026-09-02 — full tables on #1242). The flag's flip-ON condition was "A0 clears the QSV
  `MFXVideoENCODE_Reset`". It does not clear:
  - `encoder_inplace_rate=1` fails the Reset outright (`hevc_qsv Error during resetting:
    incompatible video parameters`), in **both** `low_power` modes — the VDENC fixed-function path
    and the VME path fail byte-identically, so `low_power` is ruled out as the cause.
  - The rebuild path that in-place was meant to replace costs **1.3–2.0 s at ≤ 1 Mbps** and
    340–390 ms above 1.5 Mbps, which is the ~2 s pump freeze #1254 then moved off-thread.
  - ⚠️ **`encoder_inplace_rate` must stay OFF for QSV.** Flipping it fleet-wide would break every
    QSV rate move, on the encoder the constrained population actually runs.
  - 🔑 **Consequence for A4, which the deletion list did not anticipate:** the coarsen ladder is
    **load-bearing for QSV**, because QSV has no cheap apply path at all. NVENC is settled (A2: 20
    IDRs → 0, apply 0.004 ms) and can shed its rationing; QSV cannot until it has its own answer.
    A4 is therefore **per-backend, not a flat delete** — see the amended A4 below.

### Why the Reset fails — two explanations tried, both dead

⚠️ **The hypothesis previously recorded on #1242 is refuted by our own open path.** It supposed we
open QSV *quality-driven* (`global_quality`, ICQ), so that `TargetKbps`/`MaxKbps` "aren't the
governing params and MSDK rejects resetting them", and proposed testing it with a sweep that omits
`global_quality`. But `build_encoder` sets `bit_rate = maxrate_bps` for every non-NVENC backend and
the option dict sets `maxrate` to the same value, so `qsvenc.c::select_rc_mode` takes its **CBR**
branch (`rc_max_rate == bit_rate`) before it ever reaches ICQ. `global_quality` is inert there. In
CBR those two fields are exactly what governs, so the stated mechanism cannot be the cause — and the
proposed experiment would have removed an already-inert option and produced a null that looked like
a refutation of something else.

🔑 **The replacement, grounded in the code rather than in inference: the third field.** The in-place
arm writes `bit_rate`, `rc_max_rate` **and** `rc_buffer_size`. `qsvenc` maps `rc_buffer_size` onto
`mfxInfoMFX::BufferSizeInKB`, which sizes an internal bitstream buffer allocated at Init, and our
value is `target × hrd_pct` so it moves on **every** rate change. The oneVPL spec defines
`MFX_ERR_INCOMPATIBLE_VIDEO_PARAM` as *"Reset requires additional memory allocation and cannot be
executed"* — which is what changing that field asks for. It also explains the fact `low_power` ruled
out: an allocation constraint is identical on VDENC and VME, which is exactly what was measured.

🚨 **…and that replacement is REFUTED TOO — measured on 0.4.62, CORPLAP-1, 2026-09-04.** The write
was gated behind a knob so both arms could run from one released binary, and skipping it changes
nothing: the Reset still fails `-14`. Three runs, all `hevc_qsv`, `low_power=1`, `encoder_inplace_rate=1`:

| run | open | first in-place move | `rc_buffer_size` | result |
|---|---|---|---|---|
| A (write restored) | 3 Mbps | → 6 Mbps | rewritten 3 M → 6 M | **-14** |
| B (write skipped) | 3 Mbps | → 6 Mbps | held at 3 M | **-14** |
| B′ (write skipped, clean) | **6 Mbps** | → **4.5 Mbps** (a DECREASE) | held at 6 M — generous and valid | **-14** |

B′ is the one that settles it. The buffer is untouched, larger than the new bitrate needs, and the
move is downward, so none of "the buffer changed", "the buffer grew", or "the buffer is too small
for the new rate" survives. ⚠️ The knob was **verified live** before believing the negative — the
debug line read `write_bufsize=false` on the failing run, because a flag that silently does nothing
produces exactly this result (the FR-63 slow-start trap).

⇒ **This driver rejects the bitrate change itself on Reset**, and `low_power=0` was already measured
identical, so it is not the VDENC path. An earlier sub-prediction died on the way here too: reading
oneVPL's *"requires additional memory allocation"* literally predicted that only buffer GROWTH would
fail, and the 6 Mbps-open decrease case failed identically.

**Code state: reverted.** The knob is gone and the three-field write is restored, so the in-place arm
is byte-for-byte what it was before the experiment. A lever whose hypothesis is dead is not worth a
config surface, and the finding lives here instead.

**What is left**: `mfxExtEncoderResetOption` — the FR's own untested **patch 0004**, now the leading
candidate rather than a footnote. It is an FFmpeg-level change, not reachable from a knob. If it also
fails, QSV has no in-place rate path on this driver at all, and A4 must treat `coarsen_bitrate` as
permanent for QSV rather than transitional.

⚠️ A5 (QVBR) is entangled with this and the spec should stop treating it as independent: moving QSV
off CBR changes `select_rc_mode`'s branch, so it could change whether Reset is accepted — and it is
the only route by which `global_quality` stops being inert.

**Parent/siblings:** FR-59 (the regression that motivated the arc). This is one of three FRs from that plan (FR-62 encoder apply path, FR-63 the controller, FR-64 the data path).

## Goal

A remote-desktop encoder follows the rate controller's target **without an IDR and without a rebuild**. Today `FfmpegEncoder::set_bitrate` (`agents/roomlerd/src/encode/ffmpeg/encoder.rs:3006–3136`) costs an IDR on every move: NVENC through FFmpeg's `reconfig_encoder` (n8.1.2 sets `resetEncoder=1; forceIDR=1` on a bitrate-only change), QSV/AMF/VideoToolbox through a full encoder rebuild under a comment that predates FFmpeg 6.0's runtime `update_bitrate()`. Nine of the 34 rate/quality heuristics in `encode/` exist only to ration that cost (coarsen ladder, deferred applies, 15-s thrift, background swap, settle-KF thrift, refine-vs-cap fight, opener grace, FlipTracker and kf_gate cooldowns); on LAN pairs the AIMD crosses a rung nearly every window — 44 swap IDRs in one day on one host (2026-09-02).

Also found while reading the sources: **our QSV sessions run CBR, not QVBR** (`select_rc_mode` tests `rc_max_rate == bit_rate` first and we set both to the cap), which is why every 0.4.49 frame on CORPLAP-1 was exactly `target ÷ fps ÷ 8` bytes; and the existing NVENC in-place write sizes `rc_buffer_size = target` while the open used `maxrate × hrd_pct` — every move silently resizes the HRD window.

## Key design

- **A0 — measure on silicon**: `roomlerd encoder-smoke --reconfigure-sweep` (moving-block synthetic content, 20 rate changes 6 M → 200 k → 6 M, per rung: key packets, `set_bitrate_ms`, burst, bytes-per-frame ratio). Pass = 0 key packets after the opener, apply < 5 ms, ratio ±25 % by frame 10, burst ≤ 2×. Hosts: Iris Xe (`hevc_qsv`, `low_power` 1/0), Arc (`av1_qsv`), RTX (`hevc/av1_nvenc`), Apple Silicon (`hevc_videotoolbox`); AMD recorded as unmeasured.
- **A1 — in-place applies, gated** (`encoder_inplace_rate`, default OFF in the PR, ON after A0 passes): `RateReconfig::{InPlace, Rebuild}` resolved at open; QSV writes `bit_rate + rc_max_rate + rc_buffer_size` together (CBR); the HRD sizing bug fixed; a 3 % dead-band; counters `rate_moves / rebuilds / idr_count` in the heartbeat; a startup probe per host/codec (a driver table is not evidence).
- **A2 — FFmpeg patches** carried in `.github/ffmpeg-patches/` and applied by all three vendor builders (Linux/macOS from source, Windows via the vcpkg overlay port's `PATCHES`), with a `ROOMLER-PATCHES.txt` hash gate in `release-agent.yml`: `0001-nvenc-no-idr-on-bitrate-reconfig`, `0002-videotoolbox-runtime-bitrate` (as WebRTC's VT encoder does), `0003-amf-runtime-bitrate` (disabled, unmeasured).
- **A3 — VideoToolbox/AMF** per A0. **AMF measured 2026-10-29** (Radeon 610M, 0.4.187): a 41–55 ms
  rebuild with an IDR on every rung — the D3D12/Vulkan shape, so AMF keeps `Rebuild` and patch 0003
  stays disabled; its rationing question is the IDR alone. **VideoToolbox measured 2026-11-05**
  (Apple M5 Pro, 0.4.201): a rebuild with an IDR on every rung, 8–12 ms, so it keeps `Rebuild`
  until patch 0002 exists (the open decision below). **A4 — retire** the nine rationing heuristics one shippable PR at
  a time (anchors in the spec). ⚠️ **Amended after A0-QSV**: this cannot be a flat delete. A
  heuristic that rations rate moves is dead weight on NVENC (a move is 0.004 ms and no longer costs
  an IDR) and **load-bearing on QSV** (a move is a 0.34–2.0 s rebuild). Each retirement is therefore
  gated on the backend it is retired for, and `coarsen_bitrate` in particular stays until QSV has a
  cheap apply path — deleting it because "FR-62 removed the need" would pin every QSV session to the
  rebuild cost it exists to avoid. **Moved to FR-63 on 2026-11-05**, as its B4 and AC7 (the
  Progress entry).
- **A5 (later A/B)**: QSV to QVBR (`bit_rate = 0.9 × maxrate`, keeps `global_quality`) — stops CBR padding idle frames.

## Acceptance criteria

- [x] AC1 — A0 report per backend on real hardware, attached here, with the pass/fail per codec.
      **Done for all four backends**: NVENC on an RTX 5090 (PASS — 20/20 rate-caused
      IDRs before A2, 0 after; apply 0.001–0.008 ms; re-read 0/20 on the released 0.4.187,
      2026-10-29), QSV on an Iris Xe (**FAIL** — Reset refuses in both `low_power` modes; rebuild
      0.34–2.0 s), AMF on a Radeon 610M (**FAIL** — a 41–55 ms rebuild with an IDR on every rung,
      0.4.187, 2026-10-29, read on #1242) and VideoToolbox on an Apple M5 Pro (**FAIL** — an
      8–12 ms rebuild with an IDR on every rung, 0.4.201, 2026-11-05).
- [x] AC2 — On a constrained session, under motion that saturates the cap, the bytes-per-frame ratio is ≈ 1.0 of the new rung in the first complete window after every rate move (FR-59's instrument).
      Amended 2026-11-05 on the operator's go: it read "≈ 1.0 within 2 s of every target change", a
      CBR's contract, and a target change inside a rung moves nothing.
      **Met on 0.4.201, 2026-11-05** (the Progress entry): CORPLAP-2, `av1_nvenc`, constrained
      over the overlay's DERP carrier, under a 220 s window drag. The first complete window after
      each move the drag saturated landed at 0.967, 1.001, 0.992 and 0.978 of the new rung (one
      move down, three up), and no window passed 1.228 of its cap. On a still screen after the
      drag the windows read 0.17–0.71: a quality-capped VBR spends only what the picture needs,
      so ≈ 1.0 holds where motion saturates the cap, which is how the corrected read (2026-10-31
      above) judges it. The pair carried no TURN candidate; the agent treats a DERP-carried
      overlay as constrained, as it does a TURN relay. The read: per heartbeat window,
      Δ`bytes_written` ÷ Δ`frames_encoded` against the encoder's maxrate ÷ `target_fps` ÷ 8,
      judged at the first complete window after each Δ`rate_moves`. Before it, two passive
      constrained sessions made no rate move (2026-10-31 over TURN, 2026-11-03 over DERP).
- [x] AC3 — A 5-minute constrained session's `idr_count` ≤ dims changes + viewer keyframe requests.
      **Met on 0.4.201, 2026-11-05**, the same session as AC2: 439 s, `constrained=true` on 217
      of 217 heartbeats, 13 rate moves, `rebuilds=0`. From the first heartbeat `idr_count` rose
      by 4 against 2 dims changes and 2 keyframe requests (ours and the viewer's own), each in its
      source's window, and none of the 13 moves brought one of its own. The positive controls
      registered: each resize moved `dims_swaps` and `idr_count` by exactly one, and our
      `rc:keyframe` moved `idr_count` by one. The read, per window: every Δ`idr_count` needs a
      Δ`dims_swaps`, a Δ`frames_dropped_backpressure`, a `keyframe_requests` tick, a lock edge or
      another source from the 2026-11-03 table beside it, and an unexplained IDR beside a
      Δ`rate_moves` is the RED (the corrected read, 2026-10-31 above).
- **AC4** moved to FR-63 (#1243) on 2026-11-05, the operator's call: its AC7 and phase B4 carry
      it. It read "every deleted heuristic's kill switch is retired through the registry table
      with its evidence". No heuristic had been deleted: the plan sequences every deletion after
      FR-63's B2 flip plus one release soak, and the measurements limit them to NVENC. The
      registry the plan names (`rate-control-architecture.md`, rule 4) stays in
      [`rate-control.md` § Kill switches](../rate-control.md#kill-switches-and-when-each-retires--the-registry-fr-62-ac4),
      18 switches plus the four A4 heuristics with no switch.
- [x] AC5 — `encoder_inplace_rate = false` restores today's behaviour byte-for-byte (unit test + field check).
      Unit: `encoder::tests::rate_mode_resolves_per_backend_and_flag` (OFF ⇒ `Rebuild` for QSV,
      `InPlaceVbr` for NVENC) and `encode::tests::inplace_rate_off_writes_the_pre_a1_one_times_window`
      (OFF ⇒ the 1× `rc_buffer_size` write, whatever the open's window). Field, both OFF arms live
      on the fleet default: NVENC — the dev box's 0.4.186 session (2026-10-29 10:17Z) ran
      `inplace_rate=true rebuilds=0` on the in-place arm; QSV — CORPLAP-1 on 0.4.51 (2026-09-02,
      `rate_moves=0`, the rebuild path, #1242) and on 0.4.62 (2026-09-04, two background swaps in
      five seconds, the FR-65 read).
- [x] AC6 — Docs: the rate-move arm per backend, the measured cost table, the heartbeat counters, the
      sweep's rules and the switches with their retirement conditions, in house style with a mermaid
      diagram and a `docs/README.md` row — `encoders.md` § "A rate move, per backend (FR-62)".

## Field log

| When (UTC) | Build | Where | Read |
|---|---|---|---|
| 2026-09-02 | 0.4.50 → patched 0.4.51 | RTX 5090 Laptop, `hevc_nvenc` | 20/20 rate-caused IDRs → 0/20, apply 0.001–0.008 ms (#1242) |
| 2026-09-02 | 0.4.51 | CORPLAP-1 Iris Xe, `hevc_qsv` | rebuild arm 20/20 + 340 ms–2.17 s; in-place arm `-14`, both `low_power` modes (#1242) |
| 2026-09-04 | 0.4.62 | CORPLAP-1 Iris Xe, `hevc_qsv` | the `rc_buffer_size` hypothesis refuted: `-14` with the write skipped, including a decrease from a 6 M open (#1242) |
| 2026-10-29 22:11 | **0.4.187** (released) | RTX 5090 Laptop, `hevc_nvenc` | **0/20**, 0.004 ms, default and `--constrained`; `pass:false` only on the ratio/burst artefact |
| 2026-10-29 22:12 | **0.4.187** (released) | Radeon 610M, `hevc_amf` via `--name` | 20 rebuilds, 41–55 ms each, an IDR on every rung (`first3_max` 1.4–5.4 KB vs 250–690 B trailing); reported as `periodic_idr` by the frame-0 rule |
| 2026-10-29 10:17 | 0.4.186 | dev box, `av1_nvenc` 1916×1198 @ 144, **direct** | eight AIMD steps 34 → 65 Mbps, `rate_moves=0`: the 25 M top rung swallows every step; `idr_count=2`, `dims_swaps=1` |
| 2026-10-29 22:36 | this PR's tree, built natively on the same FFmpeg pin | RTX 5090 Laptop `hevc_nvenc`, Radeon 610M `hevc_amf` | the corrected harness, same silicon, same session of readings: NVENC **`pass:true`** (0/20, 0.003–0.004 ms, `basis:idr+apply`, both HRD arms); AMF **`total_rate_idr:20, total_periodic_idr:0`**, `max_set_ms` 54.4, `pass:false` — every rung `rate_idr:1` |
| 2026-10-31 22:47 | **0.4.191** (released) | CORPLAP-2 `hevc_nvenc` 1920×1200, **relayed**, passive | `constrained=true` on 158 of 158 heartbeats over 318 s; `rate_moves=0`, `rebuilds=0`, `swaps=0`, `dims_swaps=0`; `idr_count` 18 (the opener + `dc_unopen_drops=17`), flat after the first heartbeat; 1 keyframe on the wire in 6,006 frames; bytes per frame p50 0.063, max 0.491 of the cap. No rate move, so it proves neither AC2 nor AC3 |
| 2026-11-03 22:23 | **0.4.199** (released) | CORPLAP-2 `hevc_nvenc` 1920×1200, **relayed** (the overlay's DERP carrier), passive | `constrained=true` on 57 of 57 heartbeats over 113 s; `rate_moves=0`, `rebuilds=0`, `swaps=0`, `dims_swaps=0`, no drops; `idr_count` 14 (the opener + `dc_unopen_drops=13`) → 17; 4 keyframes on the wire in 3,544 frames, three of them explained by nothing the read counted (open). No rate move, so it proves neither AC2 nor AC3. The approved motion was refused, because the OK reached the worker through another session |
| 2026-11-05 18:53 | **0.4.201** (released) | CORPLAP-2 `av1_nvenc` 1920×1080, **relayed** (the overlay's DERP carrier), a 220 s window drag, one `rc:keyframe`, a resize to 1600×1000 and back | `constrained=true` on 217 of 217 heartbeats over 439 s; `rate_moves` 0 → 13 (down to 550 k, back up to 3 M), `rebuilds=0`; `idr_count` 1 → 5 = our keyframe + 2 `dims_swaps` + the viewer's own keyframe, none from a move; 6 keyframes on the wire in 10,914 frames; the saturated windows after a move 0.967–1.001 of the new rung, at most 1.228 of the cap. **AC2 and AC3 met** |
| 2026-11-05 19:21 | **0.4.201** (released) | MacBook-1 Apple M5 Pro, macOS 27.0.1, `hevc_videotoolbox` via `--name`, as root | 20 rebuilds, 8.3–12.1 ms each (median 11.5), an IDR on every rung (`first3_max` 1.3–3.2 KB vs 0.3–0.7 KB trailing); `total_rate_idr:20, total_periodic_idr:0`, `pass:false` on `idr+apply`. A3's VideoToolbox read |

## Open decisions

- **AC2/AC3's motion**, settled 2026-11-05: the operator typed an explicit OK for the synthetic
  window drag and the positive controls into the reading session, and the read ran. An OK relayed
  through another session (2026-11-03) or one that names no action does not clear the
  automation's permission check: it has to be typed into the reading session and name the
  sequence.
- **The three 2026-11-03 IDRs** (an open question, not a decision): key frames at 22:24:37,
  22:25:28 and 22:25:44 that nothing the read counted explains. The carrier line beside them is
  ruled out (the 2026-11-03 entry). The 2026-11-05 read had none under the same overlay churn:
  its one IDR near the churn was the viewer's own request. A read that captures every tell in the
  2026-11-03 table settles it. Until then, a read rules out each of those sources before it calls
  an IDR rate-caused.
- **AC2 and AC3's wording**, settled 2026-11-05 on the operator's go. AC2 now says what the
  corrected read judges: the first complete window after each rate move, under motion that
  saturates the cap. AC3 keeps its wording: its inequality omits the opener and the drop-burst
  resync, but read from the first heartbeat, with no drop burst, it held as written.
- **VideoToolbox: patch 0002 vs. "rebuild at settle, ≥ 15 s apart".** A0 on a Mac (2026-11-05)
  read an 8–12 ms rebuild with an IDR on every move, so the rebuild's time is no argument for the
  patch, and the IDR is. The macOS builder carries no patch today, so VideoToolbox keeps the
  rebuild path and the ladder rations its IDRs, as for AMF, until someone writes patch 0002.
  Either patch (0002 here, 0004 for QSV) would widen FR-63's B4 past NVENC.
- Whether to attach `mfxExtEncoderResetOption{StartNewSequence=OFF}` (patch 0004) if any QSV driver starts a new sequence on Reset.

## Out of scope

- The controller itself (FR-63); which path ICE selects (FR-64); the QVBR switch (A5, separate A/B).

## Related

FR-59 #1163 (the regression that exposed the ladder), FR-31 (opening keyframe budget — `max_frame_size` is runtime-updatable in qsvenc and is that lever), FR-10 (relay IDR thrift, to be retired), FR-63, FR-64.
