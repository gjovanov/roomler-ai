# Handover #5 — Cursor overlay + codec pick landing in 0.1.27

> Continuation of HANDOVER4 (which scoped all Phase 1 + Phase 2
> deferred work after 0.1.26 shipped). This handover captures the
> state after the 2026-04-20 continuation session that landed the
> real OS cursor overlay (1E.*) and the agent-side codec pick half
> of 2B.2.

## Shipped in 0.1.27 (3 commits on top of 0.1.26)

```
d09a9a5  feat(rc): agent-side codec intersection + forward browser_caps      (2B.2 half)
f065734  feat(agent+ui): real OS cursor overlay                              (1E.1 + 1E.2 + 1E.3)
<this>   chore: bump workspace 0.1.26 → 0.1.27 + HANDOVER5                    (this commit)
```

agent-v0.1.26 CI built successfully on windows-latest (run 24656577426);
agent-v0.1.27 will ship these three on top.

## Verification

- `cargo clippy --workspace -- -D warnings` clean on `--features full-hw`
- `cargo test -p roomler-agent --lib --features full-hw` → 39 passed
  (+7 new for `pick_best_codec`)
- `cargo test -p roomler-ai-remote-control --lib` → 23 passed
- `cd ui && bun run test:unit` → 287 passed
  (+3 new for `base64ToBytes`)
- `cd ui && bunx vue-tsc --noEmit` clean, `bun run build` clean
- `encoder-smoke --encoder hardware` unchanged: 1 keyframe +
  9 P-frames, 4212 bytes total

## What's still open after 0.1.27

| Task | Status | Why deferred |
|---|---|---|
| 2B.2 encoder swap + SDP munging | ⏳ | `TrackLocalStaticSample` → `TrackLocalStaticRTP` is a breaking refactor of the existing H.264 path; ships paired with 2C.1 |
| 2C.1 MF HEVC backend | ⏳ | Needs RFC 7798 RTP packetizer (~400 LoC — webrtc-rs ships H.264 only). The encode pipeline itself is ~200 LoC copy of `mf/sync_pipeline.rs` with HEVC GUIDs |
| 2C.3 AV1 path | ⏳ | Depends on 2B.2 + 2C.1 foundation. draft-ietf-avtcore-rtp-av1-07 packetizer is a separate substantial implementation |
| 2C.2 VideoToolbox HEVC | ⏳ | macOS only; untestable on this Win11 box |
| 1A.2 Intel QSV async pipeline | ⏳ | No Intel QSV on this dev box (RTX 5090 Laptop + AMD Radeon 610M) |
| 1C.1 WGC capture backend | ⏳ | ~500 LoC of Win32_Graphics_Capture FFI + WinRT apartment handling |
| 1C.3 GPU downscale | ⏳ | Depends on 1C.1 |

All tracked in HANDOVER4.md with design notes + effort estimates.
The honest read: Phase 1 sub-phases A/B/D/F/G and Phase 2
sub-phases A/B are **done**. Phase 2 completion (actually
negotiating + streaming HEVC/AV1) is the remaining payload, and
it's a coherent 1-2 week chunk of focused encoder + RTP work.

## Ship order for 0.1.27

```bash
# Push this commit, tag, let CI build the MSI.
git push origin master
git tag agent-v0.1.27
git push origin agent-v0.1.27

# Once MSI is built and the server is deployed with matching
# schemas (AgentResponse.capabilities + ClientMsg::SessionRequest.
# browser_caps + ServerMsg::Request.browser_caps), install on
# controlled hosts via the remote-server.txt recipe.
ssh mars && cd /home/gjovanov/roomler-ai-deploy && \
    ansible-playbook deploy.yml     # server first
# then: uninstall 0.1.26 MSI, download + install 0.1.27 MSI
```

## Behavioural changes vs 0.1.26

1. **Real OS cursor in the browser.** When the agent runs
   `--features full-hw` on Windows, the controller sees the
   actual mouse cursor bitmap (arrow, I-beam, hand, resize, etc.)
   instead of the initials badge. Linux/macOS agents fall back to
   the badge since the tracker is Windows-only today.
2. **Codec selection logged.** The agent logs the chosen codec
   for every session based on browser ∩ agent capability
   intersection. Today this is purely observational — the
   H.264 track is still what's actually sent. Once 2B.2 + 2C.1
   land, the same selection drives the encoder + RTP track.
3. **Extra data channel**: `cursor` (reliable+ordered). Older
   controllers that don't open it still work — agent only spawns
   the pumper on `on_data_channel` delivery.

## Things that surprised me during 1E.* implementation

1. **`HCURSOR` and `HICON` are distinct newtypes in windows-rs**
   even though Win32 typedef'd them to the same `HANDLE`. Had
   to wrap manually: `GetIconInfo(HICON(hcursor.0), ...)`.
2. **Monochrome cursors are rare but real.** Classic Windows
   mono cursors have `hbmColor` null — the mask bitmap stacks
   AND+XOR vertically so real height = `bmHeight/2`. Emit synthesised
   black-with-alpha pixels in that case; users would see a solid
   black outline, which is fine for rare OEM cursors.
3. **GetIconInfo returns owned HBITMAP handles.** Leaking them
   exhausts the process-wide GDI handle pool within ~10k polls.
   Always call `DeleteObject` on both `hbmColor` and `hbmMask`
   after extracting the bits.
4. **HCURSOR reuse is stable per-session.** Windows keeps a
   small pool of cursor handles and recycles them; caching by
   raw pointer value is safe for the life of a session. Browsers
   see the same `shape_id` every time the agent reports the
   standard arrow — so the shape bitmap is only sent once per
   session.

## Next-session priority if continuing

**2B.2 + 2C.1 together, as a single release.** The work:

1. Move H.264 from `TrackLocalStaticSample` to
   `TrackLocalStaticRTP`. Write an H.264 RTP packetizer (RFC 6184
   single-NAL + FU-A fragmentation + STAP-A aggregation — webrtc-rs's
   internal packetizer is one file, translate directly).
2. Add HEVC RTP packetizer (RFC 7798: single-NAL, FU, AP modes;
   DONL/DOND for B-frames but we don't use B-frames so skip).
3. New `encode/mf/hevc.rs` — copy `mf/sync_pipeline.rs`, change
   output-type GUID to MFVideoFormat_HEVC, skip CABAC enable, wire
   through `EncoderPreference` selection.
4. `peer.rs`: pick the codec from `browser_caps` intersection
   (already computed by `caps::pick_best_codec`), build the
   matching encoder + RTP track, munge the SDP offer to list only
   the chosen codec (or just the preferred order).
5. End-to-end test: Chrome controller, Quality: High, agent on
   Windows with HW HEVC MFT → H.265 session with ~40% bitrate
   reduction vs H.264 at equal quality.

Estimated: 3-5 days of focused work. Big prize: the plan's
1440p60 at 8-12 Mbps number becomes reachable.

## Files new in 0.1.27

- `agents/roomler-agent/src/capture/cursor.rs` — CursorTracker +
  Windows tracker + ARGB extraction
- `ui/src/components/admin/agentCodecChips.ts` — pure helper
  extracted from AgentsSection so vitest can cover it (was in
  0.1.26, listed here because I described it in the last handover
  under 2A.2 but didn't explicitly list as new)

Modified files tracked in each commit's `git show`.
