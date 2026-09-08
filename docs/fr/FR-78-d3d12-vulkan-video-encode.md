# FR-78 — D3D12 and Vulkan video encode: vendor-neutral hardware cells on Windows and Linux

**Issue:** [#1503](https://github.com/gjovanov/roomler-ai/issues/1503) · **Status:** proposed 2026-09-08 — spec, not started · **Opened:** 2026-09-08 · **Glossary:** [`CONTEXT.md`](../../CONTEXT.md) · **ADR:** [0001](../adr/0001-encoder-backends-compiled-in-discovered-at-runtime.md) · **Related:** [FR-77](FR-77-encoder-chroma-matrix.md) · [FR-62](FR-62-encoder-rate-changes-without-an-idr.md) · [`docs/encoders.md`](../encoders.md)

## Goal

Two more **backends** in the one runtime-probed build per platform (FR-77's shape,
ADR 0001), both vendor-neutral and both loaded at runtime with nothing new to ship
or depend on:

- **D3D12 video encode** (`h264_d3d12va`, `hevc_d3d12va`, `av1_d3d12va`; FFmpeg ≥ 8.1)
  on Windows — any GPU whose driver implements the D3D12 video-encode DDI, which
  today is every NVIDIA, AMD and Intel desktop driver in support.
- **Vulkan video encode** (`h264_vulkan`, `hevc_vulkan`, `av1_vulkan`; FFmpeg ≥ 7.1)
  on Linux and Windows — NVIDIA (R550+), AMD (RADV, Mesa 24.1+) and Intel (ANV,
  Mesa 24+) through one API, with 4:4:4 decided at runtime on HEVC and AV1.

The **cell** vocabulary, the picker, the probe, the cache and the denylist are
FR-77's; this FR adds names to the cascade tables and the two hardware-frame
uploads the pump does not have yet. Nothing about how a session is chosen changes.

## Why

- **The vendor SDK is a dependency the host may not have.** AMF's runtime
  (`libamfrt64.so.1` / `amfrt64.dll`) ships with AMD's proprietary driver only —
  jupiter and zeus (AMD Raphael) reach hardware encode through VAAPI on Linux and
  would reach nothing through AMF on a Windows box with the in-box driver. QSV
  needs Intel's media runtime; NVENC needs `libcuda`/`nvEncodeAPI`. D3D12 and
  Vulkan video encode ride the **graphics driver every box already has**.
- **The cascade already routes around a broken vendor path** (RTX 5090: MF's
  `ActivateObject` fails, NVENC works). A vendor-neutral rung under the vendor
  ones turns "the SDK is missing or broken" from "software encode" into "the same
  silicon through another door".
- **Measured size** (FR-77 §Why, the full 8.1.2 static library): D3D12 video
  encode 83 KB linkable + the 349 KB of CBS bitstream writers the D3D12 / VAAPI /
  Vulkan wrappers share (already linked on Linux for VAAPI). Under 1 MB against a
  35 MiB binary — the same measurement that closed the per-vendor-build question.
- **Both are runtime-loaded by FFmpeg itself**: `hwcontext_d3d12va.c` loads
  `d3d12.dll` / `dxgi.dll` with `LoadLibrary`, `hwcontext_vulkan.c` loads
  `vulkan-1.dll` / `libvulkan.so.1` with the loader in `vulkan_loader.h`. No new
  DT_NEEDED, no new `Depends` — the P4 libva lesson (a load-time need of the
  daemon binary is a fleet-freeze hazard on the offline `dpkg --install` path) does
  not recur.

## Key design

1. **Cascade positions** (`agents/roomlerd/src/encode/ffmpeg/encoder.rs`, the
   `*_ENCODER_NAMES` tables): vendor SDKs → `*_videotoolbox` → `*_vaapi` →
   **`*_d3d12va` → `*_vulkan`**, closing every table. A host with a working vendor
   SDK keeps its vendor path first (the FR-62 rate-control behaviour of each
   backend is known; the new ones' is not). The order-lock tests grow two names.
2. **Hardware frames, one shape** (`encode/ffmpeg/vaapi.rs` today): the VAAPI
   `Device` (once per process) + `Frames` (pool per encoder, `sw_format` NV12 /
   VUYX, upload with `av_hwframe_get_buffer` + `av_hwframe_transfer_data` +
   `av_frame_copy_props`) is the same sequence for `AV_HWDEVICE_TYPE_D3D12VA` and
   `AV_HWDEVICE_TYPE_VULKAN`. Generalise the module over the device type; device
   candidates per type (D3D12: the adapter LUID that owns the primary output, the
   one SystemContext already picks; Vulkan: the physical device, `ROOMLERD_VULKAN_DEVICE`).
3. **The vendor builds**: Windows overlay port adds `--enable-d3d12va` (the
   Windows SDK headers are on the runner) and `--enable-vulkan` (the Vulkan SDK
   headers, `vulkan-headers` from vcpkg); the Linux from-source job adds
   `--enable-vulkan` with `libvulkan-dev`'s headers only (FFmpeg dlopens the
   loader). New asset names on both, as P4 did, so a rollback is a suffix.
4. **Cells and the probe**: `VideoBackend::{D3d12, Vulkan}` in
   `crates/remote_control/src/models.rs` (wire strings `d3d12`, `vulkan`; older
   readers ignore unknown names by the FR-77 rule); `hw: true` by construction;
   `hevc_vulkan:yuv444` and `av1_vulkan:yuv444` join the built-in denylist until a
   driver proves the open, as every packed / RExt cell did.
5. **Rate control**: both backends take `b:v` / `maxrate` / `bufsize` and reject
   nothing the tiered open cannot fall through; whether a bitrate change forces an
   IDR is measured with FR-62's `encoder-smoke --ladder` before a cell leaves the
   denylist, never assumed.

## Phases

| # | Phase | Kill switch | Status |
|---|---|---|---|
| P0 | Vendor builds with `--enable-d3d12va` (Windows) and `--enable-vulkan` (Windows + Linux); the runtime probe asserts the new names; new asset names | the asset pattern in `release-agent.yml` | — |
| P1 | The hardware-frame module generalised over the device type; D3D12 and Vulkan device + pool + upload; `encoder-smoke` real bytes on the dev box (NVIDIA, both), CORPLAP-3 (Intel D3D12) and jupiter (RADV Vulkan) | `ROOMLERD_USE_FFMPEG=0` / the denylist | — |
| P2 | Cells, cascade positions, the probe's 4:4:4 candidates for `hevc/av1_vulkan`, the FR-62 ladder read per cell | the denylist | — |
| P3 | Field: sessions on each backend from the viewer, the operator-judged text scroll on the 4:4:4 cells that open | — | — |
| P4 | `docs/encoders.md` (the tables and the cascade diagram), `docs/README.md` row | — | — |

## Acceptance criteria

- [ ] **P0** — both vendored trees carry the new names; the Linux `.deb`'s `Depends`
      and bundle are unchanged (no new load-time library); the Windows MSI grows by
      less than 1 MB.
- [ ] **P1** — `encoder-smoke` produces real bytes through `hevc_d3d12va` on the dev
      box and CORPLAP-3, and through `hevc_vulkan` on the dev box and jupiter; a host
      without a Vulkan loader or a D3D12 video driver fails the open in one line and
      keeps its other cells.
- [ ] **P2** — the server records of the dev box, CORPLAP-3 and jupiter carry the
      new cells with `hw: true` and unchanged vendor cells; the picker offers them
      with the right reasons; a session on each cell reports its chroma in `rc:video-info`.
- [ ] **P3** — the FR-62 ladder read per new cell (IDR on bitrate change: yes/no)
      recorded; the operator-judged scroll on every 4:4:4 cell that opens.
- [ ] **Docs** updated with the new backends in the tables and diagrams, linked
      from `docs/README.md`.

## Open decisions

- Whether `*_d3d12va` should sit BEFORE Media Foundation for H.264 on Windows: MF
  is the older path with the RTX 5090 quirk, D3D12 reaches the same silicon — the
  answer is a measurement (latency and IDR behaviour on the dev box), not a design.
- Vulkan on Windows: NVIDIA and AMD ship `VK_KHR_video_encode_*` there; whether it
  is worth a rung under D3D12 on the same box is decided by whether any host opens
  Vulkan and not D3D12 (expected: none).

## Out of scope

- Decode of any kind (the browser decodes; nothing on the agent side decodes).
- Replacing the vendor SDK rungs — they stay first while their behaviour is the
  known one.
- Linux arm64 (no FFmpeg there) and macOS (VideoToolbox is the OS's answer).

## Field-verification log

| Date | Where | Phase | Read |
|---|---|---|---|
| 2026-09-08 | FR-77 §Why (the full 8.1.2 static library, win64) | — | D3D12 video encode h264/hevc/av1: 83 KB linkable; the CBS writers shared with VAAPI/Vulkan: 349 KB; `*_vulkan` HEVC/AV1 list 4:4:4 as runtime-decided. The sizes that make this FR a runtime question, not a build one |
