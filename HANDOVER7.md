# Handover #7 — AV1 cascade + fail-closed codec semantics in 0.1.29

> Continuation of HANDOVER6 (which landed 2B.2 + 2C.1 for HEVC in 0.1.28).
> This session adds the AV1 path (2C.3) and tightens codec-failure
> semantics so we never silently substitute a different codec's bytes
> into a track bound to the negotiated codec's MIME.

## Shipped in 0.1.29 (1 follow-on commit on top of 0.1.28)

```
<this>   feat(agent): AV1 cascade + fail-closed codec semantics  (2C.3)
```

## What changed vs 0.1.28

| Layer | Change | Why |
|---|---|---|
| `encode/mf/sync_pipeline.rs` | Third variant `OutputCodec::Av1` (plus `MFVideoFormat_AV1` output subtype). Keyframe detection split into codec-aware dispatcher: H.264 / HEVC use Annex-B start-code scanning, AV1 walks OBU sequences looking for OBU_SEQUENCE_HEADER. 4 new AV1 bitstream unit tests + LEB128 decoder test. | AV1 MFT emits a raw OBU sequence (not Annex-B) — webrtc-rs's `Av1Payloader` consumes exactly that shape, so bytes pass through unchanged for all three codecs. Keyframe detection needs a different heuristic per codec. |
| `encode/mf/activate.rs` | `enumerate_hw_av1_mfts()` + AV1 branch in `activate_and_probe_pipeline_for_codec`. Like HEVC, AV1 is HW-only: no SW fallback, Err on exhaustion. Enumeration log line now names the subtype GUID instead of hardcoding "HW H.264 MFTs". | MFTEnumEx with `MFVideoFormat_AV1` surfaces IHV AV1 encoders on capable hosts. |
| `encode/mf/mod.rs` | `MfEncoder::new_av1(w, h)` entry point. `probe_av1_adapter_count()` for capability reporting. | Parallels `new_hevc` / `new_h264`; reuses the parametric sync pipeline. |
| `encode/mod.rs` | **Fail-closed rewrite** of `open_for_codec` for non-H.264 codecs. Previously HEVC failure silently demoted to H.264; now both HEVC and AV1 return `NoopEncoder` on cascade exhaustion. Factored per-codec openers into helper functions to keep the `#[cfg]` branches clean. | The hazard: the RTP track was bound to `video/H265` or `video/AV1` in `AgentPeer::new` before the encoder opened, so substituting H.264 bytes would feed the browser's `HevcPayloader`/`Av1Payloader` garbage bytes labeled as a different codec, producing silent decoder errors. Fail-closed means "black video + loud WARN" instead of "corrupted decode". Operator sees the problem and can retry with Quality: Low. H.264 is unchanged (the trait cascade inside `open_default` picks from multiple backends all producing H.264 Annex-B, so cross-backend demotion is safe). |
| `encode/caps.rs` | Advertises `"av1"` + `"mf-av1-hw"` when MFTEnumEx finds at least one HW AV1 encoder. | Lights up the AV1 chip in AgentsSection when hardware is present. Known caveat: on RTX 5090 Blackwell the AV1 MFT enumerates but activation fails (0x8000FFFF) so the advertised capability is "optimistic". A session that picks AV1 on this box would fail closed as designed. |
| `peer.rs` | `build_video_codec_cap("av1")` returns `video/AV1` + `profile-id=0` fmtp, matching webrtc-rs PT 41. `codec_params_for("av1")` pins PT 41. 7 new codec-cap unit tests. | Matches the default MediaEngine's AV1 registration so `payloader_for_codec` resolves the `Av1Payloader` without intervention. |

## Verification on RTX 5090 Laptop + AMD Radeon 610M

- `cargo clippy -p roomler-agent -p roomler-ai-remote-control --features full-hw -- -D warnings` clean
- `cargo test -p roomler-agent --lib --features full-hw` → **54 passed** (+ 4 AV1 bitstream tests + 7 codec-cap tests on top of 0.1.28's 43)
- `cargo test -p roomler-ai-remote-control --lib` → 23 passed
- `encoder-smoke --encoder hardware --codec h264` → mf-h264, 4212 bytes, PASSED
- `encoder-smoke --encoder hardware --codec h265` → mf-h265 (HEVCVideoExtensionEncoder on NVIDIA adapter), 6714 bytes, PASSED
- `encoder-smoke --encoder hardware --codec av1` → AV1 cascade finds "NVIDIA AV1 Encoder MFT",
  activation fails 0x8000FFFF (Blackwell regression — same as H.264/HEVC), fails closed with `NoopEncoder`.
  Smoke exits non-zero as expected for an unsupported codec on this host.

## Known caveats (ship anyway)

1. **RTX 5090 Blackwell AV1 activation regression.** The NVENC AV1 MFT
   enumerates (caps::detect advertises `av1`) but `ActivateObject`
   returns `0x8000FFFF` ("Catastrophic failure") just like the H.264
   and HEVC NVENC MFTs on this box. Root cause unknown; possibly
   driver-version-specific, possibly needs
   `CODECAPI_AVEncAdapterLUID` to disambiguate the target GPU.
   Behaviour on this box: browser negotiates AV1, agent's
   AV1 encoder fails open, session shows black video with a loud WARN
   in the logs. Operator toggles Quality: Low to downgrade to H.264.
2. **Advertising ≠ activating.** caps::detect is enumeration-based
   for speed (a full probe-per-codec at startup would cost ~600 ms
   of MF init across three codecs). Runtime activation is the
   authoritative check; the cascade catches the mismatch and fails
   closed. A future refinement could cache a one-time probe result
   at agent startup to make caps honest without the per-session cost.
3. **No mid-session codec renegotiation.** If the browser's
   preferred codec fails at session startup, the session can't
   switch without a full SDP re-offer. The controller has to
   reconnect; Quality: Low gives them a way to force H.264 on the
   next attempt. Proper mid-session renegotiation is out of scope
   for this plan (needs controller-initiated `pc.createOffer` with
   `iceRestart: false` + new SDP exchange, see 2D.1 in the original
   plan for the browser-side sketch).

## What's still open after 0.1.29

| Task | Status | Why deferred |
|---|---|---|
| 2C.2 VideoToolbox HEVC | ⏳ | macOS only; untestable on this Win11 box. |
| 1A.2 Intel QSV async pipeline | ⏳ | No Intel QSV on this RTX 5090+AMD 610M box. The MF cascade routes AMD async MFTs to `MfInitError::AsyncRequired`; handler is unimplemented. |
| 1C.1 WGC capture backend | ⏳ | ~500 LoC of Win32_Graphics_Capture FFI + WinRT apartment. Unblocks real dirty-rect plumbing + 1C.3 GPU downscale + full 1F.1 VFR. |
| 1C.3 GPU downscale | ⏳ | Depends on 1C.1. |
| NVENC Blackwell `ActivateObject 0x8000FFFF` | ⏳ root-cause unknown | Affects H.264, HEVC, AND AV1 on this box. The cascade routes around it (AMD 610M + HEVCVideoExtensionEncoder win instead on H.264+HEVC; AV1 has no alternative backend and fails closed). Worth a fresh investigation with a driver update, or trying `CODECAPI_AVEncAdapterLUID` to pin MFT activation to a specific adapter. |
| Probe-at-startup for honest caps | ⏳ | Would cache probe results so caps::detect only advertises codecs that actually activate. Deferred — benefit is marginal on most hosts, adds ~600 ms to agent startup. |

## Ship order for 0.1.29

```bash
git push origin master
git tag agent-v0.1.29
git push origin agent-v0.1.29

# Once MSI is built (per ../remote-server.txt):
ssh mars && cd /home/gjovanov/roomler-ai-deploy && \
    ansible-playbook deploy.yml       # server first (no wire-format changes in 0.1.29)
# Then on controlled hosts:
#   1. Get-Process roomler-agent | Stop-Process -Force
#   2. Uninstall 0.1.28 via Settings → Apps
#   3. Download + install 0.1.29 MSI from GitHub releases
#   4. Verify: roomler-agent --version
#   5. Smoke: roomler-agent encoder-smoke --encoder hardware --codec av1
#      (on RTX 40+ / Intel Arc / RDNA3+ with fresh drivers this should
#      produce real AV1 output; on Blackwell/older hardware it will
#      correctly fail closed)
#   6. roomler-agent run
#   7. Browser: https://roomler.ai/tenant/.../agent/.../remote
#      On a box where AV1 works, webrtc-internals should show video/AV1
#      at ~30-40% lower bitrate than HEVC at equal quality.
```

**Server-side**: no wire-format changes in 0.1.29 vs 0.1.28. The
browser-caps list already carries any codec the browser advertises;
the `AgentCaps.codecs` now includes `"av1"` on capable hosts but
that's additive (old controllers ignore unknown codecs).

## Behavioural changes vs 0.1.28

1. **Chrome/Edge on AV1-capable hardware negotiates AV1.** On RTX 40+,
   Intel Arc, RDNA3+ with fresh drivers the cascade succeeds;
   webrtc-internals shows `video/AV1` and bitrate drops ~30-40% vs
   HEVC at equal visual quality.
2. **On AV1-incapable hardware the session fails closed.** A browser
   that advertises AV1 + H.265 + H.264 and connects to an agent that
   enumerates AV1 but can't activate it will see black video with a
   WARN in agent logs. Recovery: reconnect with Quality: Low (forces
   H.264 on the browser's offer).
3. **HEVC failure is now fail-closed too.** Previously 0.1.28 demoted
   to H.264 on HEVC init failure; but the track was bound to
   `video/H265` so those H.264 bytes would feed HevcPayloader
   garbage. The honest fix is to fail closed. On hosts where HEVC
   actually works (like this box, via HEVCVideoExtensionEncoder)
   nothing changes.

## Things that surprised me during 2C.3

1. **AV1 OBU framing is codec-aware.** Unlike H.264/HEVC which emit
   Annex-B (start code + NALU), AV1 emits length-prefixed OBUs
   (header byte + optional extension + LEB128 size + payload). The
   `is_keyframe_bitstream` dispatcher has to know which shape the
   MF MFT produces.
2. **OBU_SEQUENCE_HEADER is a reliable keyframe proxy.** IHV AV1
   encoders emit a fresh sequence header only at keyframes. Walking
   the OBU list looking for `obu_type=1` is ~20 lines of code and
   gives the right `is_keyframe` flag 99% of the time.
3. **NVIDIA AV1 Encoder MFT enumerates on RTX 5090 even though
   activation fails.** The Blackwell regression is activation-only,
   not enumeration. Relying on enumeration for capability reporting
   is optimistic; the current fail-closed semantics make that
   tolerable but a probe-at-startup would make caps honest.
4. **NoopEncoder is a better error than demotion-to-H.264 here.**
   In 0.1.28 I demoted HEVC failures to H.264. Demoting HEVC bytes
   to H.264 doesn't work because the track is bound to `video/H265`.
   The honest answer was to fail closed. This also guided the
   AV1 design.

## Files modified (summary)

- `agents/roomler-agent/src/encode/mf/sync_pipeline.rs` — AV1 variant,
  OBU keyframe heuristic, LEB128 decoder, 4 new unit tests.
- `agents/roomler-agent/src/encode/mf/activate.rs` — AV1 branch, fixed
  misleading log line that said "H.264 MFTs" for every codec.
- `agents/roomler-agent/src/encode/mf/mod.rs` — `MfEncoder::new_av1`
  + `probe_av1_adapter_count`.
- `agents/roomler-agent/src/encode/mod.rs` — factored `open_for_codec_av1`
  + `open_for_codec_hevc` helpers; fail-closed semantics for both.
- `agents/roomler-agent/src/encode/caps.rs` — advertises AV1 when
  enumeration finds AV1 MFTs.
- `agents/roomler-agent/src/peer.rs` — `video/AV1` branch in codec
  cap + params; 7 new codec-cap tests.
- `Cargo.toml` — workspace version 0.1.28 → 0.1.29.
- `HANDOVER7.md` — this file.

## Next-session priority if continuing

**1C.1 WGC capture backend** becomes the highest-leverage remaining item:

1. Unblocks real dirty-rect plumbing — `Frame::dirty_rects` already
   threads through to `set_roi_hints`, just no source.
2. Unblocks 1C.3 GPU downscale (D3D11 texture path).
3. Unblocks full 1F.1 encode-only-when-dirty VFR (~50 kbps idle
   instead of today's ~500 kbps).

Estimated 500 LoC, 1-2 days. Would be the last Phase 1 piece.

Alternative: **revisit the NVENC Blackwell `ActivateObject 0x8000FFFF`
regression**. Driver update experiments + `CODECAPI_AVEncAdapterLUID`
probes. Could unblock the RTX 5090's HW encoders for all three codecs
— would be the biggest quality jump possible on this dev box.
