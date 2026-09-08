# FR-78 — D3D12 and Vulkan video encode: vendor-neutral hardware cells on Windows and Linux

**Issue:** [#1503](https://github.com/gjovanov/roomler-ai/issues/1503) · **Status:** P0 + P1 built (#1506) — the dev box advertises 13 cells; the roll and the field reads on CORPLAP-3 (Intel D3D12) and jupiter (RADV Vulkan) follow · **Opened:** 2026-09-08 · **Glossary:** [`CONTEXT.md`](../../CONTEXT.md) · **ADR:** [0001](../adr/0001-encoder-backends-compiled-in-discovered-at-runtime.md) · **Related:** [FR-77](FR-77-encoder-chroma-matrix.md) · [FR-62](FR-62-encoder-rate-changes-without-an-idr.md) · [`docs/encoders.md`](../encoders.md)

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

## P0 + P1 — as built (#1506)

- **The vendor trees.** Windows: the vcpkg overlay port takes the port's own `vulkan`
  feature (vcpkg's `vulkan-headers` 1.4.357 ≥ the 1.4.317 `av1_vulkan` needs), re-states
  `--enable-d3d12va` (what the baseline port already passes) and `--enable-vulkan`, and
  enables `h264/hevc/av1_d3d12va` + `h264/hevc/av1_vulkan`; the verify step asserts the
  sixteen encoder symbols and that `avutil.lib` carries **no link directive** for
  `vulkan-1.lib` / `d3d12.lib` — FFmpeg dlopens both at runtime, so the static tree grows
  no import and the agent no load-time need. Linux: the Vulkan C headers from the Khronos
  repo at v1.4.362 into the prefix (22.04's `libvulkan-dev` is 1.3.204), `--enable-vulkan`
  + `--extra-cflags=-I/opt/ffout/include`, the three `*_vulkan` encoders (seventeen in
  the runtime probe), and an assert that no vendored lib DT_NEEDs `libvulkan`. The
  `spirv-headers` warning configure prints is about swscale, which is off. Both assets
  carry a new suffix (`-d3d12-vulkan`, `-vaapi-vulkan`); rollback = drop it.
- **One hardware-frame module** (`encode/ffmpeg/hwframes.rs`, the P4 `vaapi.rs`
  generalised): `HwKind::{Vaapi, D3d12, Vulkan}` keyed by the encoder name's suffix, with
  per-kind candidate devices (VAAPI: the pinned node, then `/dev/dri/renderD128`…`135`;
  D3D12: the pinned `d3d12_adapter`, then DXGI adapter indices `0`…`3` — FFmpeg's device
  string; Vulkan: the pinned `vulkan_device` by index or name, then physical devices
  `0`…`3`). **The open decides the device** (`open_on_some_device`): each candidate is
  opened lazily and once and kept for the process, the encoder's whole configure-and-open
  runs against the preferred device first and then the rest until one succeeds, and the
  winner is remembered so a session lands where the probe proved. Measured reason, dev
  box: Vulkan device 0 (the Radeon 610M iGPU) has no `VK_KHR_video_encode_queue`, the RTX
  5090 does; D3D12 adapter 0 opens HEVC but an RDNA2 iGPU has no AV1 — which device can run
  a codec is a per-codec, per-driver fact only the encoder's own open answers.
  `Frames::new(dev, kind, sw_format, w, h)` sets the pool's `format` to the kind's hardware
  pixel format; the upload is the same three calls for all three. The pixel format goes
  onto the codec context raw — ffmpeg-next's `Pixel` enum has no D3D12 variant. 4:4:4
  forms: packed VUYX on VAAPI, planar `yuv444p` on Vulkan (the driver's format list
  decides at open), never on D3D12 (NV12 / P010 only, so `*_d3d12va` is not on the
  4:4:4-capable list).
- **Options.** D3D12: `rc_mode=VBR` (uppercase, like VAAPI's — `vbr` reads as an
  undefined constant and drops the tier, measured) on the cap with the HRD window, `bf=0` (FFmpeg's
  d3d12va default is `bf=2` — two frames of reordering delay a remote desktop cannot
  carry), `async_depth=1` tier-protected. Vulkan: the same, `profile=rext` for HEVC
  4:4:4, and the Vulkan tuning / usage hints (`tune=ull`, `usage=stream`) in the
  tier-protected group with `async_depth=1`, so a driver that rejects them costs the knob,
  never the open.
- **Cells.** `VideoBackend::{D3d12, Vulkan}` (wire `d3d12`, `vulkan`; older readers
  ignore unknown names), `from_ffmpeg_name` splits `_d3d12va` / `_vulkan`, `hw: true` by
  construction; the probe's 4:4:4 candidates include Vulkan cells; `hevc_vulkan:yuv444`
  joins the built-in denylist. The cascade tables close `… → videotoolbox → vaapi →
  d3d12va → vulkan` (locked by the order tests).

## Phases

| # | Phase | Kill switch | Status |
|---|---|---|---|
| P0 | Vendor builds with `--enable-d3d12va` (Windows) and `--enable-vulkan` (Windows + Linux); the runtime probe asserts the new names; new asset names | the asset pattern in `release-agent.yml` | **shipped** #1506 → `agent-v0.4.88` (vendor run 34217171639): Windows `…-minimal-d3d12-vulkan.zip` (16 encoder symbols, no `vulkan-1.lib` / `d3d12.lib` directive in `avutil.lib`), Linux `…-minimal-vaapi-vulkan.tar.xz` (17 encoders in the runtime probe, no `libvulkan` DT_NEEDED, Vulkan-Headers 1.4.362 in the tree); MSI +254 KB |
| P1 | The hardware-frame module generalised over the device type; D3D12 and Vulkan device + pool + upload; the cells on the dev box (NVIDIA, both), CORPLAP-3 (Intel D3D12) and jupiter (RADV Vulkan) | `ROOMLERD_USE_FFMPEG=0` / the denylist | **shipped** #1506 → `agent-v0.4.88`, **field-read 2026-09-08**: the dev box advertises 13 cells (D3D12 hevc/h264, Vulkan hevc/av1/h264 on the RTX after the iGPU refused), CORPLAP-3's Intel iGPU `hevc/d3d12` (its H.264/AV1 D3D12 and its Vulkan refused by the driver), WSL / zeus / mars / the MacBook unchanged; jupiter's Vulkan cell needs `RADV_PERFTEST=video_encode` (open decision) |
| P2 | Cells, cascade positions, the probe's 4:4:4 candidates for `hevc/av1_vulkan`, the FR-62 ladder read per cell | the denylist | — |
| P3 | Field: sessions on each backend from the viewer, the operator-judged text scroll on the 4:4:4 cells that open | — | — |
| P4 | `docs/encoders.md` (the tables and the cascade diagram), `docs/README.md` row | — | — |

## Acceptance criteria

- [x] **P0** — both vendored trees carry the new names (16 / 17 encoders asserted by
      the vendor jobs); the Linux `.deb`'s `Depends` and bundle are unchanged (no new
      load-time library — asserted: no `libvulkan` DT_NEEDED, no `vulkan-1.lib` /
      `d3d12.lib` directive); the Windows MSI grew by 254 KB on `agent-v0.4.88`.
- [ ] **P1** — the cells open on real silicon: the dev box advertises `hevc/h264` on
      D3D12 and `hevc/h264/av1` on Vulkan (the RTX 5090, after the iGPU refused), and
      CORPLAP-3's Intel iGPU advertises `hevc/d3d12` — both from the probe's real open;
      a host without a Vulkan encode queue or a D3D12 video driver fails the open in one
      line and keeps its other cells (CORPLAP-3's Vulkan, WSL's software ICDs, mars).
      Still owed: `encoder-smoke` real bytes through the new backends (the cascade picks
      the vendor SDK first, so the smoke needs a way to name the backend — P2), and the
      Linux Vulkan cell on jupiter, which needs `RADV_PERFTEST=video_encode` in the
      daemon's environment.
- [ ] **P2** — the server records of the dev box, CORPLAP-3 and jupiter carry the
      new cells with `hw: true` and unchanged vendor cells; the picker offers them
      with the right reasons; a session on each cell reports its chroma in `rc:video-info`.
- [ ] **P3** — the FR-62 ladder read per new cell (IDR on bitrate change: yes/no)
      recorded; the operator-judged scroll on every 4:4:4 cell that opens.
- [ ] **Docs** updated with the new backends in the tables and diagrams, linked
      from `docs/README.md`.

## Open decisions

- **RADV exposes Vulkan video encode only under `RADV_PERFTEST=video_encode`** (Mesa
  25.2.8 on jupiter: zero video extensions without it, `VK_KHR_video_encode_{queue,h264,h265}`
  with it — measured with `vulkaninfo`). The daemon could set that in its own
  environment before the first Vulkan open and light the cells on every AMD Linux host;
  it does not, deliberately — an experimental driver path enabled fleet-wide on k8s
  bare-metal hosts is the operator's call, not the probe's. The lever today is a systemd
  drop-in (`Environment=RADV_PERFTEST=video_encode`); a config key that makes the daemon
  set it is the P2 candidate.
- **The probe cache key does not see driver environment.** It hashes the `ROOMLERD_*`
  knobs; `RADV_PERFTEST` / `LIBVA_DRIVERS_PATH` change the answer and do not change the
  key, so an operator who edits the drop-in must clear `caps-cache.json` or the old answer
  is reused. P2: add the driver-facing variables to the knob hash.
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
| 2026-09-08 | vendor run 34217171639 | P0 | Windows: the vcpkg port's `vulkan` feature + `--enable-d3d12va` (already the port's) → `…-minimal-d3d12-vulkan.zip`, sixteen encoder symbols, no `vulkan-1.lib` / `d3d12.lib` directive in `avutil.lib`; `avcodec.lib` 12.1 → 21.8 MB (the CBS writers + the two wrappers, static — the MSI delta is the number that matters, read after the release). Linux: Vulkan-Headers 1.4.362 into the prefix, `--enable-vulkan`, seventeen encoders in the runtime probe, no `libvulkan` DT_NEEDED; the tree 1.64 → 1.84 MB packed (the headers ride along) |
| 2026-09-08 | the dev box's WSL2 (Mesa 25.2.8, `libvulkan1` 1.3.275, ICDs incl. `dzn`), the P1 build against the new tree | P1 | **The Vulkan device path works up to the frames pool, and WSL is a negative cell for Vulkan encode.** `hwframes: device opened backend="vulkan" device="(default)"` — the loader picked Mesa's `dzn` (D3D12-backed, "not a conformant Vulkan implementation, testing use only"), then `av_hwframe_ctx_init(vulkan)` failed `Operation not supported (-95)`: no video frames on dzn, the name fails in one line, the NVENC cells are untouched. The dzn device open costs ~2.5 s in the probe (`probe_ms` 2005 → 4665 on this box; the probe cache pays it once per build). `*_d3d12va` on Linux: `not registered` before any device attempt, as designed |
| 2026-09-08 | the dev box (RTX 5090 Laptop + Radeon 610M, Windows 11), the P1 build against the new tree, native | P1 | **Both backends open on real silicon, and the open had to choose the device.** D3D12: adapter `0` opened; `hevc_d3d12va` opened with the full low-latency tier (`rc_mode=VBR maxrate bufsize bf=0 async_depth=1`) → cell in 83 ms; `h264_d3d12va` → cell in 16 ms; `av1_d3d12va`: adapter 0 (the RDNA2 iGPU) "does not support VBR RC mode" / no compatible RC mode, adapter 1 (the RTX) "Driver does not support requested features … Codec configuration not supported", adapter 3 (WARP) no RC mode ⇒ not advertised, correctly. Vulkan: device `0` opened and `hevc_vulkan` refused it three tiers deep — `Device does not support the VK_KHR_video_encode_queue extension!` (the iGPU) — then device `1` (the RTX) opened with the full tier (`rc_mode=vbr … tune=ull usage=stream async_depth=1`) → **preferred from now on**; `hevc_vulkan` 474 ms, **`av1_vulkan` 13 ms**, `h264_vulkan` 12 ms. The matrix: **13 cells** (the 0.4.87 eight + `hevc/d3d12`, `hevc/vulkan`, `av1/vulkan`, `h264/d3d12`, `h264/vulkan`), probe 5550 ms (4327 on 0.4.87; the device walk is the difference, paid once per build by the cache); the 4:4:4 phase unchanged (`hevc_vulkan:yuv444` denied). First run also found the `rc_mode` spelling: `vbr` is "Undefined constant" to d3d12va (uppercase `VBR`, as VAAPI) — the open fell to defaults and still succeeded, which is what the tiers are for |
| 2026-09-08 | release + the fleet roll | P0 / P1 | `agent-v0.4.88` (bump #1507 → `cc180d979`; release run 34223695163, 28 assets). **Sizes**: MSI 15,822,848 → 16,076,800 B (**+254 KB**, the < 1 MB criterion met), x86_64 `.deb` 13,721,956 → 13,804,128 B (+82 KB), arm64 unchanged; the `.deb`'s `Depends` and bundle unchanged (the vendor jobs' no-DT_NEEDED asserts hold). Rolled to all seven hosts through the update route; six came back on 0.4.88 within minutes — jupiter's install collided with an `apt-get` I was running on it for `vulkan-tools` (`Could not get lock /var/lib/dpkg/lock-frontend`; both `apt-get` and `dpkg` candidates failed, the daemon stayed alive and the re-push hit the install cooldown), retried below |
| 2026-09-08 | the dev box, 0.4.88 (server record) | P1 / P2 | **13 cells advertised**: `hevc/nvenc` 4:2:0+4:4:4 · `hevc/amf` · **`hevc/d3d12`** · **`hevc/vulkan`** · `av1/nvenc` · **`av1/vulkan`** · `h264/nvenc` 4:2:0+4:4:4 · `h264/amf` · **`h264/d3d12`** · **`h264/vulkan`** · `h264/mf` · `h264/openh264` · `vp9/libvpx`; `probe_ms` 6051 (4327 on 0.4.87 — the device walk, cached after the first start) |
| 2026-09-08 | CORPLAP-3 (Intel Meteor Lake iGPU `8086:7d45`, Windows), 0.4.88 — the server record + the probe child by hand under `roomler exec` | P1 / P2 | **`hevc/d3d12` is a new cell on Intel** — `Using device 8086:7d45 (Intel(R) Graphics)`, `hevc_d3d12va` opened with the full low-latency tier in 84 ms, and the host now advertises `h265` (QSV never offered HEVC there). `h264_d3d12va`: `Failed to check encoder support (887a0020)` on the first tier, then `Failed to check rate control support / Driver does not support VBR RC mode`, then no compatible RC mode — the Intel D3D12 driver refuses FFmpeg's H.264 configuration; the QSV cell covers H.264. `av1_d3d12va`: `Driver does not support VBR RC mode` on the iGPU, adapter 1 = the Basic Render Driver refuses to open (`-22`), adapters 2–3 the same RC refusal — QSV covers AV1. Vulkan: device 0 (the Intel driver) has no `VK_KHR_video_encode_queue`, devices 1–3 do not exist — no Vulkan cells on Intel Windows, correctly. `probe_ms` 6811 (5660 on 0.4.87) |
| 2026-09-08 | the WSL sibling · zeus · mars · the MacBook, 0.4.88 (server records) | P1 | WSL: the three NVENC cells unchanged, `probe_ms` 5866 (2508 on 0.4.87 — four software Vulkan ICDs opened and refused; cached after the first start). zeus (RADV without `RADV_PERFTEST=video_encode`): `hevc/vaapi` + `h264/vaapi` unchanged, `probe_ms` 200 — no Vulkan cell, as the extension list predicts. mars: `openh264` + `libvpx`, 501 ms. MacBook: `hevc/videotoolbox` + `h264/videotoolbox` unchanged, 120 ms — the new names are inert on the platform whose FFmpeg carries none of them |
| 2026-09-08 | jupiter, `vulkaninfo` | P1 | **RADV exposes video encode only under `RADV_PERFTEST=video_encode`**: Mesa 25.2.8 (`RADV RAPHAEL_MENDOCINO`, Vulkan 1.4.318) lists zero `VK_KHR_video_*` extensions by default and `VK_KHR_video_encode_{queue,h264,h265}` with the flag. The positive Linux Vulkan cell therefore needs the daemon's environment (a systemd drop-in) — see *Open decisions* |
