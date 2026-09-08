# FR-80 — a session with no pixels says why

**Issue:** [#1532](https://github.com/gjovanov/roomler-ai/issues/1532) · **Status:** proposed 2026-09-08 · **Glossary:** [`CONTEXT.md`](../../CONTEXT.md) · **Related:** [FR-27](FR-27-host-consent-prompt-surfaces.md) · [FR-77](FR-77-encoder-chroma-matrix.md) · [`docs/remote-control.md`](../remote-control.md)

## Goal

When a session produces no pixels, the operator learns **why, in the viewer**,
within the time it takes to notice. Two halves, both proven missing in the field
on 2026-09-08:

1. **The agent names the cause.** A capture backend that cannot open today
   degrades to `NoopCapture`, writes one `WARN` to its own log and tells the
   controller nothing. The controller can only show a stall.
2. **The viewer never invents a codec.** The status pill's no-`rc:video-info`
   fallback prints `VP9 <chroma>` unconditionally, so a session on any other
   codec that never delivers a frame reports as VP9.

## Why — the field event that produced it

The MacBook stopped streaming on 2026-09-08. Every session negotiated correctly
and painted black. The operator's read was "the codec picker is broken: I force
H.264 and it picks up VP9 4:4:4, and HEVC does not work either" — a reasonable
conclusion from what the product showed, and wrong.

What actually happened, in order:

- The macOS release began shipping under a **Developer ID** signature that day
  (`Developer ID Application: G ROX EOOD (4TG7586MY5)`), replacing the interim
  self-signed identity. macOS binds a TCC grant to the signature, so the
  host's existing Screen Recording permission stopped matching:
  `Failed to match existing code requirement for subject com.roomler.agent and
  service kTCCServiceScreenCapture` — the stored requirement named the old
  certificate root, the binary presented the Developer ID leaf.
- `scrap::Capturer` therefore failed to open. `capture::open_default` logged
  `scrap capture unavailable — falling back to NoopCapture error=… other error`
  and returned a capturer that yields nothing.
- The pump started correctly: `media pump: H.264 over DataChannel` →
  `FFmpeg DC pump starting codec_label="H264"`. No frame ever reached the
  encoder, so **no `rc:video-info` was ever sent**, so the viewer fell back to
  its VP9 label and reported `VP9 4:4:4` for an H.264 session.
- The viewer saw only `media stalled 6 s` → `media stalled 10 s, keyframe probe
  unanswered — re-creating session`, forever.

Two independent defects compounded into a misdiagnosis: the host knew the answer
and did not say it, and the viewer asserted something it had not been told. The
same shape as FR-27's `no_prompt_surface` (an unattributable failure that reads
as a different one), and the same remedy: **name the cause on the wire**.

⚠️ This will recur on **every** macOS host at its next update, and the class
recurs whenever a signing identity changes. The permission itself is the
operator's to restore; the product's job is to say so.

## Key design

1. **The reason survives the fallback.** `capture::open_default`
   (`agents/roomlerd/src/capture/mod.rs:490`) currently drops the backend's
   error on the floor and returns `Box::new(NoopCapture)`
   (`mod.rs:683`). `NoopCapture` carries a `CaptureUnavailable { code, detail }`
   instead, exposed through one new `ScreenCapture` trait method defaulting to
   `None`, so no call site changes shape and every pump can ask.
2. **The code is derived, never guessed.** `code` is a small closed set —
   `permission` · `no_display` · `not_built` · `backend_error`. On macOS a
   failed open asks the OS (`CGPreflightScreenCaptureAccess`) rather than
   pattern-matching an error string, because scrap reports a bare
   `other error` and a guess would be a second lie. On Linux an absent
   `DISPLAY`/`WAYLAND_DISPLAY` is `no_display`. Anything unattributed stays
   `backend_error` with the backend's own text — an honest "we do not know".
3. **One new control-DC message**, `rc:media-unavailable`, built by a sibling of
   `video_info_payload` (`peer.rs:4475`) and sent with the same
   retry-until-delivered discipline both pumps already use for `rc:video-info`
   (rc.87: a once-only send races the control-DC open and is silently lost).
   It is peer-to-peer JSON on the control channel, **not** a `ClientMsg` /
   `ServerMsg` variant, so the server wire, the namespace table and the
   composition baseline are untouched. An older viewer ignores an unknown `t`.
4. **The pill stops asserting.** The fallback label derives from the transport
   the viewer itself negotiated instead of a hardcoded `VP9`, and a session with
   no frame yet says so rather than naming a codec as if it were running. The
   legacy libvpx path — the one case where the old fallback was right — keeps
   its label because that transport genuinely is VP9.
5. **The viewer surfaces the reason where the operator is looking**: on the
   black canvas, not only in the console, with the macOS case naming the
   setting to change.

## Phases

| # | Phase | Kill switch | Status |
|---|---|---|---|
| P1 | `CaptureUnavailable` on `NoopCapture` + the trait accessor; the macOS/Linux derivation | — (a pure addition; absent reason = today's behaviour) | — |
| P2 | `rc:media-unavailable` on the control DC, retry-until-delivered, from both pumps | an old viewer ignores the message | — |
| P3 | The viewer: the reason on the canvas, and the pill's fallback stops naming VP9 | pure UI | — |
| P4 | Docs — `docs/remote-control.md` (the message + the reason table), `docs/encoders.md` cross-ref | — | — |

## Acceptance criteria

- [ ] **P1** — a host whose capture cannot open reports a `code` that matches
      the actual cause, with a positive control: the same host with capture
      working reports no reason at all.
- [ ] **P2** — the message arrives at the viewer on a session that never
      produces a frame, including on a relay session where the control DC opens
      late (the rc.87 race the retry exists for).
- [ ] **P3** — an H.264 session that never delivers a frame **never** displays
      "VP9" anywhere in the viewer; a libvpx VP9 session still reads VP9; a
      healthy session is unchanged and still names its real encoder.
- [ ] **Field** — reproduced on a host with no capture (a headless Linux fleet
      node is the natural negative cell) and read in the viewer; and the
      MacBook, permission restored, streams with the pill naming the real
      encoder.
- [ ] **Docs** updated in the house style and linked from `docs/README.md`.

## Out of scope

- Restoring the permission itself. A remote agent must not be able to grant
  itself screen capture, and nothing here tries.
- Making `has_input_permission` track the OS rather than the build
  (`encode/caps.rs:1654` records that gap). Same class, separate change.
- Re-signing or pinning the macOS identity so TCC grants survive a
  certificate change. That is FR-7's ground, and the answer there is that a
  Developer ID move is a one-time cost.
