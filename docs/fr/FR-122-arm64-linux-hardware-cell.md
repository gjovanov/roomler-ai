# FR-122: A hardware encoder cell on arm64 Linux, proven in a session

**Issue:** [#2875](https://github.com/gjovanov/roomler-ai/issues/2875) · **Status:** proposed 2026-10-25 — spec
and claim; parked for a host, nothing to build for NVENC · **Owner:** the agent's encoder surface · **Anchors:**
master `a886539e7` · **Split from:** [FR-97](FR-97-os-feature-parity.md) P7a, by its D18 (the operator,
2026-10-25: "ship AAC now, GPU later")

## 1. Goal

An arm64 Linux host with a hardware video encoder advertises that cell, and a remote-desktop session opens it.
This is the half of FR-97's P7a that the lab could not prove (FR-97 AC14's first clause).

## 2. Evidence

- **P7a ships the encoders.** FR-97 P7a vendors FFmpeg 9.0.1 for arm64 (`vendor-ffmpeg-windows.yml`,
  `build-linux-arm64-minimal`): `h264/hevc/av1_nvenc`, `h264/hevc/av1/vp9_vaapi`, `h264/hevc/av1_vulkan`, the
  h264 decoder and the aac encoder. The arm64 `.deb` and tarball bundle it, and the child-process probe decides
  per host whether any of it opens.
- **The lab's arm64 host has no encoder.** It is an Asahi M2 Pro. Asahi's GPU driver serves 3D (OpenGL, Vulkan),
  and Apple's video encoder has no Linux driver; Asahi's latest progress report covers decoding only. On P7a's
  candidate there, the probe advertised the software cells alone (`openh264-sw`, `libvpx-vp9-444-sw`), as on
  0.4.169. Recordings went from Opus to AAC (FR-97 §8, 2026-10-25).
- **The rentable host was blocked.** An AWS G5g (Graviton2 + NVIDIA T4G) runs NVENC on arm64 Linux, which
  FFmpeg's `nvenc` drives unchanged. The account offered refused it: an organisation policy explicitly denies
  `ec2:CreateInternetGateway`, and its only VPC is a private corporate network that a test box must not join.
  Every resource created there was removed the same hour (FR-97 §8).

## 3. Key design

There is nothing to build for NVENC: P7a's FFmpeg carries it, `libnvidia-encode` is dlopen'd, and the cascade
already dispatches `*_nvenc` on Linux. What is missing is a host and a session, because **a probe proves an open,
and only a session proves a cell** (the `encoder-cells` skill; `docs/encoders.md`).

| Host | What it needs | Cost |
|---|---|---|
| AWS G5g (T4G) in an account without the internet-gateway deny | nothing in the agent; an isolated VPC for ~2 h, then deleted | a few dollars |
| NVIDIA Jetson | a backend: its encoder is not FFmpeg's `nvenc` | a board + M |
| RK3588 (rkmpp), Raspberry Pi 4 (V4L2 M2M) | a V4L2 M2M backend, the only route to SoC encoders | a board + M |

## 4. Phases

| Phase | What | Kill switch | Lab target | Size |
|---|---|---|---|---|
| **P0** | A host: the operator's choice of the rows above | — | — | — |
| **P1** | On an NVENC host: the released arm64 build's hello names `ffmpeg-h264_nvenc` (and `hevc`), a session opens it, and a recording uses it with AAC | `encoder_cells_deny` | the host from P0 | S |
| **P2** | Only if P0 picks a SoC board: a V4L2 M2M backend | its own deny cell | the board | M |
| **P3** | Docs, below | — | — | S |

## 5. Acceptance criteria

- [ ] **AC1.** An arm64 Linux host advertises a hardware cell (`hw_encoders` names it) from the release build.
- [ ] **AC2.** A session opens that cell. The agent log names the backend, the viewer receives frames, and the
  cell is recorded as proven.
- [ ] **AC3.** A recording on that host uses the hardware cell and writes AAC.
- [ ] **AC4 — docs.** `docs/encoders.md` (the arm64 cells), `docs/platform-support.md` row A3 and the user
  page, with this FR's row in `docs/README.md`'s index.

## 6. Open decisions

1. **Which host** (P0): an AWS account of the operator's own, or lab hardware (and with it P2).

## 7. Out of scope

- Apple's encoder under Linux: it waits on an Asahi driver.
- Windows ARM64 and Intel Macs (FR-97 D6).

## 8. Field-verification log

| Date | Build | Set-up | Result |
|---|---|---|---|
| — | — | — | nothing yet: no host |
