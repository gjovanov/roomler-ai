# FR-62 — Encoder rate changes without an IDR

**Issue:** [#1242](https://github.com/gjovanov/roomler-ai/issues/1242) · **Status:** A0 measured on BOTH silicon — NVENC passes, **QSV in-place is broken and the flag stays OFF**; **both** recorded causes are REFUTED (the second by measurement on 0.4.62); patch 0004 is what is left; A1 shipped inert; A2 + const flip shipped (0.4.51) · **A3 — AMF measured** on the Radeon 610M (0.4.187, 2026-10-29): a rebuild on every rung, **41–55 ms, 20/20 IDRs** — the 0.4.187 harness filed them as periodic, fixed in the same PR; VideoToolbox still unmeasured · NVENC re-read on the released 0.4.187: **0/20** · docs: [`encoders.md` § A rate move, per backend](../encoders.md#a-rate-move-per-backend-fr-62) · the kill-switch registry (AC4's instrument): [`rate-control.md` § Kill switches](../rate-control.md#kill-switches-and-when-each-retires--the-registry-fr-62-ac4) · **Plan:** `rate-control-architecture` (approved 2026-09-02)

## Progress

- **2026-10-31 — AC2/AC3's read corrected, AC4's registry built, AC4's sequencing recorded.**
  - 🔑 **On NVENC the default build already moves the rate in place, with no IDR.**
    `resolve_rate_mode` (`agents/roomlerd/src/encode/ffmpeg/encoder.rs:868`) returns
    `InPlaceVbr` for every `*_nvenc` whatever `encoder_inplace_rate` says, and
    `NVENC_RECONFIG_FORCES_IDR` (`:893`) has been `false` since patch 0001 shipped
    (0.4.51). On NVENC the flag only sizes the VBV window that a move writes
    (`inplace_rate_bufsize`, `encode/mod.rs:355`). So a constrained NVENC session needs
    no config change to be read for AC2/AC3, and flipping the flag off does not produce
    a RED (no IDR per rate step). The read's RED is a **positive control**: one viewer
    resize (a dims change) must move `idr_count` by exactly one. A read that cannot
    see that IDR cannot vouch for the absence of the others.
  - **The cell.** The only NVENC devices in the test org are the dev box (the
    operator's own machine, never a test target) and CORPLAP-2, a corporate laptop
    whose `av1_nvenc` sessions relay over its VPN without any pin. So AC2/AC3 wait for
    the operator to choose the cell; the other test hosts are AMD VAAPI, a
    VideoToolbox Mac and a server with no GPU.
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

A remote-desktop encoder follows the rate controller's target **without an IDR and without a rebuild**. Today `FfmpegEncoder::set_bitrate` (`agents/roomlerd/src/encode/ffmpeg/encoder.rs:1726–1671`) costs an IDR on every move: NVENC through FFmpeg's `reconfig_encoder` (n8.1.2 sets `resetEncoder=1; forceIDR=1` on a bitrate-only change), QSV/AMF/VideoToolbox through a full encoder rebuild under a comment that predates FFmpeg 6.0's runtime `update_bitrate()`. Nine of the 34 rate/quality heuristics in `encode/` exist only to ration that cost (coarsen ladder, deferred applies, 15-s thrift, background swap, settle-KF thrift, refine-vs-cap fight, opener grace, FlipTracker and kf_gate cooldowns); on LAN pairs the AIMD crosses a rung nearly every window — 44 swap IDRs in one day on one host (2026-09-02).

Also found while reading the sources: **our QSV sessions run CBR, not QVBR** (`select_rc_mode` tests `rc_max_rate == bit_rate` first and we set both to the cap), which is why every 0.4.49 frame on CORPLAP-1 was exactly `target ÷ fps ÷ 8` bytes; and the existing NVENC in-place write sizes `rc_buffer_size = target` while the open used `maxrate × hrd_pct` — every move silently resizes the HRD window.

## Key design

- **A0 — measure on silicon**: `roomlerd encoder-smoke --reconfigure-sweep` (moving-block synthetic content, 20 rate changes 6 M → 200 k → 6 M, per rung: key packets, `set_bitrate_ms`, burst, bytes-per-frame ratio). Pass = 0 key packets after the opener, apply < 5 ms, ratio ±25 % by frame 10, burst ≤ 2×. Hosts: Iris Xe (`hevc_qsv`, `low_power` 1/0), Arc (`av1_qsv`), RTX (`hevc/av1_nvenc`), Apple Silicon (`hevc_videotoolbox`); AMD recorded as unmeasured.
- **A1 — in-place applies, gated** (`encoder_inplace_rate`, default OFF in the PR, ON after A0 passes): `RateReconfig::{InPlace, Rebuild}` resolved at open; QSV writes `bit_rate + rc_max_rate + rc_buffer_size` together (CBR); the HRD sizing bug fixed; a 3 % dead-band; counters `rate_moves / rebuilds / idr_count` in the heartbeat; a startup probe per host/codec (a driver table is not evidence).
- **A2 — FFmpeg patches** carried in `.github/ffmpeg-patches/` and applied by all three vendor builders (Linux/macOS from source, Windows via the vcpkg overlay port's `PATCHES`), with a `ROOMLER-PATCHES.txt` hash gate in `release-agent.yml`: `0001-nvenc-no-idr-on-bitrate-reconfig`, `0002-videotoolbox-runtime-bitrate` (as WebRTC's VT encoder does), `0003-amf-runtime-bitrate` (disabled, unmeasured).
- **A3 — VideoToolbox/AMF** per A0. **AMF measured 2026-10-29** (Radeon 610M, 0.4.187): a 41–55 ms
  rebuild with an IDR on every rung — the D3D12/Vulkan shape, so AMF keeps `Rebuild` and patch 0003
  stays disabled; its rationing question is the IDR alone. VideoToolbox remains the open decision
  below (a Mac with an Apple Silicon encoder runs the same sweep). **A4 — retire** the nine rationing heuristics one shippable PR at
  a time (anchors in the spec). ⚠️ **Amended after A0-QSV**: this cannot be a flat delete. A
  heuristic that rations rate moves is dead weight on NVENC (a move is 0.004 ms and no longer costs
  an IDR) and **load-bearing on QSV** (a move is a 0.34–2.0 s rebuild). Each retirement is therefore
  gated on the backend it is retired for, and `coarsen_bitrate` in particular stays until QSV has a
  cheap apply path — deleting it because "FR-62 removed the need" would pin every QSV session to the
  rebuild cost it exists to avoid.
- **A5 (later A/B)**: QSV to QVBR (`bit_rate = 0.9 × maxrate`, keeps `global_quality`) — stops CBR padding idle frames.

## Acceptance criteria

- [x] AC1 — A0 report per backend on real hardware, attached here, with the pass/fail per codec.
      **Done for the three backends measured**: NVENC on an RTX 5090 (PASS — 20/20 rate-caused
      IDRs before A2, 0 after; apply 0.001–0.008 ms; re-read 0/20 on the released 0.4.187,
      2026-10-29), QSV on an Iris Xe (**FAIL** — Reset refuses in both `low_power` modes; rebuild
      0.34–2.0 s) and AMF on a Radeon 610M (**FAIL** — a 41–55 ms rebuild with an IDR on every rung,
      0.4.187, 2026-10-29, read on #1242). VideoToolbox remains unmeasured and is recorded as such
      rather than assumed; A3 still owns it.
- [ ] AC2 — On a constrained session the bytes-per-frame ratio is ≈ 1.0 within 2 s of every target change (FR-59's instrument).
      Not read yet: it needs a **relayed** NVENC pair (`constrained=true` in the heartbeat, a public
      relay candidate in the nominated pair) for five minutes with real desktop motion. The only
      sessions the dev box's agent saw on 2026-10-29 were direct (`constrained=false`), and a direct
      path at a 25 M rung is not the cell. The read: per heartbeat window, `bytes_written` ÷
      `frames_encoded` against `target_bps ÷ target_fps ÷ 8`, judged at the first window after each
      `DC pump set_bitrate (AIMD)` line. The default build is the arm under test: NVENC applies
      in place with no flag (2026-10-31 above). The only NVENC host in the test org that is not
      the dev box is CORPLAP-2 over its VPN, so the operator picks the cell.
- [ ] AC3 — A 5-minute constrained session's `idr_count` ≤ dims changes + viewer keyframe requests.
      Same session as AC2: `idr_count` against `dims_swaps` plus the session's `PLI/FIR → forcing
      keyframe` lines. (The 2026-10-29 direct session read `idr_count=2` with `dims_swaps=1` and the
      opener — consistent, but direct, so not this criterion's cell.) The positive control comes
      first: one viewer resize in the same session must move `idr_count` by exactly one.
- [ ] AC4 — Every deleted heuristic's kill switch is retired through the registry table with its evidence.
      No heuristic has been deleted yet (A4 has not started). The registry table the plan names
      (`rate-control-architecture.md`, rule 4) exists since 2026-10-31:
      [`rate-control.md` § Kill switches](../rate-control.md#kill-switches-and-when-each-retires--the-registry-fr-62-ac4),
      18 switches plus the four A4 heuristics with no switch. Blocked on FR-63's B2 flip plus one
      release soak, where the plan sequences every deletion, and on each row's own precondition.
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

## Open decisions

- VideoToolbox: patch 0002 vs. "rebuild at settle, ≥ 15 s apart" (A0 on a Mac decides).
- Whether to attach `mfxExtEncoderResetOption{StartNewSequence=OFF}` (patch 0004) if any QSV driver starts a new sequence on Reset.

## Out of scope

- The controller itself (FR-63); which path ICE selects (FR-64); the QVBR switch (A5, separate A/B).

## Related

FR-59 #1163 (the regression that exposed the ladder), FR-31 (opening keyframe budget — `max_frame_size` is runtime-updatable in qsvenc and is that lever), FR-10 (relay IDR thrift, to be retired), FR-63, FR-64.
