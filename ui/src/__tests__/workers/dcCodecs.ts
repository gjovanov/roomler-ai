// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
/**
 * FR-135 R3 — every codec the one data-channel worker (`rc-dc-worker.ts`)
 * decodes, as the composable names it in `init-canvas`. The black-box worker
 * specs run each case over all of them: one worker, so a session must behave
 * the same whichever codec it carries, and a branch on the codec string would
 * show up here.
 *
 * The strings are the composable's: `vp9DcCodec`, `AV1_CODEC_STRING`,
 * `H264_DC_CODEC_CANDIDATES[0]`, `HEVC_MAIN_CODEC_STRING`,
 * `HEVC_REXT_CODEC_STRING` (`useRemoteControl.spec.ts` locks that they agree).
 */
export const DC_CODECS = [
  { name: 'VP9 4:4:4', codec: 'vp09.01.10.08' },
  { name: 'VP9 4:2:0', codec: 'vp09.00.10.08' },
  { name: 'AV1', codec: 'av01.0.13M.08' },
  { name: 'H.264', codec: 'avc1.640034' },
  { name: 'HEVC', codec: 'hev1.1.6.L153.B0' },
  { name: 'HEVC 4:4:4', codec: 'hev1.4.10.L153.B0' },
] as const
