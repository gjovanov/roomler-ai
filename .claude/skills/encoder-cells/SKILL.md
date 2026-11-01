---
name: encoder-cells
description: The invariants that govern roomler's video-encoder surface — the capability probe (child processes, phases, cache, denylist), the codec × backend × chroma cell matrix, and the per-backend FFmpeg gotchas (NVENC, QSV, AMF, VAAPI, D3D12, Vulkan, VideoToolbox). Load BEFORE touching anything under agents/roomlerd/src/encode/, the caps/hello wire, encoder cascades or chroma selection, before adding or denying a codec/backend cell, when a host advertises no hardware or a session paints garbage, or when changing the vendored FFmpeg build. FR-77 / FR-78.
---

# Encoder cells and the capability probe

Reference doc: **[`docs/encoders.md`](../../../docs/encoders.md)** — the full
matrix, cascade tables and field log. This file is the set of invariants that
are cheap to break and expensive to have broken.

## 1 · The probe is a child process, and a failure costs a cell or the hardware

`encode::caps::detect()` spawns `roomlerd caps-probe` (hidden subcommand) and
reads one `ROOMLER_CAPS_JSON:{…}` line. A `base` child that dies inside an open
costs that cell and nothing else (below, #3267). A failure with no cell to blame
— two deaths between opens, unspawnable, out of runs or time — means *every
hardware codec is unavailable*: a last child proves the software cells, and if
that fails too it falls back to `compute_caps(false)`.

A capability probe is untrusted third-party code by definition (vendor drivers,
GPU firmware) and does not belong in the daemon's address space. Before this, a
fault inside a probe took `roomlerd` down and the service manager restarted it
straight back into the same probe — a **crash-loop**, not a degraded agent.

⚠️ `ROOMLERD_HW_AUTO=0` / `ROOMLERD_ENCODER=software` do **not** skip the probe.
Those pick an *encoder*; the probe enumerates what to **advertise**.
⚠️ The child sets `ROOMLERD_CAPS_CHILD=1`; `detect()` seeing it computes
in-process — that is the recursion guard.
⚠️ stdout is **marker-parsed**, not last-line-parsed, so anything else logging
there cannot be mistaken for the result. The child's stderr is **inherited on
purpose** so per-codec probe lines land in the daemon log next to the verdict.
⚠️ **The child never `println!`s** — every line goes through `child::emit`
(#2719). On an update the parent is terminated mid-probe, the report meets a
closed pipe, and `println!` panics there: the panic hook then filed an agent
crash on every such update. A closed pipe now exits `EXIT_PARENT_GONE` (3) and
records nothing.
⚠️ **The child ends with the worker** (#2717). On Windows it joins a
`KILL_ON_JOB_CLOSE` job that the parent holds while it waits
(`child::tie_to_this_process`). A worker that dies mid-probe (terminated,
crashed, or leaving at its 3 s runtime-shutdown budget, #2718) must not orphan
a `roomlerd.exe caps-probe`. It would hold the binary an update replaces, and
nobody would enforce the 60 s bound on a child hung in a driver.

### Anything derived from CONFIG belongs in the PARENT

The child does **not** inherit the process-local S2 config-fallback registry
(`tunnel_core::env::register_config_fallbacks`). `child::probe()` exports every
registered knob as a real `ROOMLERD_*` env var
(`config_fallbacks_for_child`, precedence preserved) and `detect()` recomputes
`caps.rpc = rpc_caps()` in the **parent**.

Before that, a config-only `relay_server_enabled = true` started the relay server
in-process while the hello never advertised `relay-server`, so the FR-19 mint
could only ever answer `no_relay`.

### Two children, not one (0.4.85)

| Phase (`ROOMLERD_CAPS_PHASE`) | Opens |
|---|---|
| `base` | every 4:2:0 open + the software cells + the vp9_qsv IDR verdict |
| `444` | only the 4:4:4 forms of `candidates_444`, answered as `ROOMLER_CAPS_444:[…]`, folded in by `merge_444` |
| `gpu` (FR-118 P2-3) | only with `zero_copy` on: the GPU-input forms of `candidates_gpu` (QSV 4:2:0), each opened on the capture adapter's device and fed two GPU frames, answered as `ROOMLER_CAPS_GPU:[…]` |
| `software` (#3267) | only once `base` has given up: libvpx's cell and nothing else, answered as `base` answers, so the host keeps `data-channel-vp9-444` |

The 0.4.84 roll found the cost of one child: a laptop's Intel media runtime died
with `0xc0000005` on the first `vp9_qsv` 4:4:4 open and the daemon advertised
**no hardware at all**. A dying 4:4:4 child now costs only the 4:4:4 forms.

⚠️ **`base` is still one process, so a dead child costs the cell it died in**
(#3267, `child::run_base`, `docs/encoders.md`). AMD's driver from 32.0.21043 faults
on `hevc_vulkan`, and a Radeon laptop lost its RTX 5090's NVENC too, with a viewer
that looked healthy. `base` now announces each open and its end
(`ROOMLER_CAPS_PROGRESS:-`), and the parent runs it again, at most 9 times and
none started after 90 s:

- **The same named cell, a fault's status, twice in a row: denied and cached.**
  The first death adds nothing to the next run, so a transient passes it.
- **A hang, or any other named death twice: denied for this process only**, and the
  result is not cached. An OOM kill or an EDR's terminate says nothing about the
  open it landed in, and proving a hang costs another minute.
- **No cell to blame: one more run**, then it gives up: no hardware, as before, but a
  last child (`software`) still proves libvpx, which needs no driver. Only if that
  dies too is the answer `compute_caps(false)`.
- ⚠️ A report the child printed stands however it ended afterwards.
- ⚠️ 255 counts as a fault on Windows because the field says so: 2 of 7 AMD deaths
  read it. Don't "tidy" `windows_fault_code` down to the exception codes.

When a host is missing a cell, read `learned_deny` in the log or
`capabilities.unavailable["encoder-cells"]`, then the WER event's faulting module.
`roomlerd caps-probe` by hand with `ROOMLERD_CAPS_CHILD=1 ROOMLERD_CAPS_PHASE=base`
reproduces a fault in seconds, and its last `PROGRESS` line names it. A healthy
session is no tell, because an MF H.264 session plays with no hardware cell at all.

⚠️ **The child's stderr reaches no log on a service host.** It announces every
open on stdout (`ROOMLER_CAPS_PROGRESS:<name>:yuv444`, line-buffered so it
survives the crash) and the parent quotes the last one — *that* is the
`encoder_cells_deny` entry to add (`<name>:gpu` → `zero_copy_deny`).

⚠️ **A form no child has opened never opens in the daemon.** A session's GPU
input reads the `gpu` child's answer at every open (`gpu_input::plan` →
`unproven`); the probe child is the only process exempt, because proving the
form is its job. A new input form gets a phase of its own on the same terms.

## 2 · The cache

`caps-cache.json` next to the daemon's logs (`encode/caps_cache.rs`), keyed on
exactly what it depends on: the **build** (crate version + exe length/mtime), the
**hardware fingerprint** (`encode/hwid.rs`), and a **SHA-256 of every `ROOMLERD_*`
knob**. Any mismatch, a format bump, or **7 days** re-probes. `caps_cache = false`
/ `ROOMLERD_CAPS_CACHE=0` switches read and write off. macOS has no key ⇒ no
cache (its probe is ~120 ms).

⚠️ **Only a result with a HARDWARE cell is cached.** A no-hardware answer is the
cheap case *and* the one a service that starts before the display driver
produces; freezing it for a week would be a silent fleet regression.
⚠️ A hit takes **only** the driver-derived fields (`hw_encoders`, `codecs`,
`transports`, `hevc_chroma`, `vp9_chroma`, `video_cells`). Permissions, the
GUI-session state and every verb list are recomputed by the running process
(`caps::merge_cached`) — they change without any driver changing.
⚠️ On a Windows SCM host the **worker** (console session) runs the probe and owns
the cache file, not the supervisor's `service-logs`.
⚠️ **A confirmed learned deny lives in this file and nowhere else** (#3267), and an
answer carrying one stands for a day, not a week. Any key mismatch drops it, and the
next probe tests the cell again: that is how a vendor's fix heals with nobody acting.
A result leaning on an unconfirmed deny is never cached.

## 3 · The denylist is a kill switch — it must apply at EVERY entry point

`ROOMLERD_ENCODER_CELLS_DENY` (`name:chroma`, comma-separated, empty = deny
nothing) **replaces** the built-in list. A denied cell is never opened — by the
probe **and** by a session, both reading `cells::names_420` / `names_444`.

⚠️ Until 0.4.90 only the probe read it: the 4:2:0 session cascades handed the
static table to the dispatcher, and a HEVC session opened a denied `hevc_vaapi`
while the hello advertised only the Vulkan cells.

> **A kill switch a child process honours and the daemon ignores keeps nothing
> out of the daemon.**

⚠️ **The learned list (#3267) is not this list.** `cells::denied_cells` is the
operator's list plus what the probe learned, so the key keeps replacing the
built-in, clearing it still goes back to it, and nothing learned is ever written
to the config. `none` does not reach a learned cell; `caps_learn_deny = false` does.
Sessions read it through the same `names_420` / `names_444` and `new_preferred`;
Media Foundation (`h264_mf`, `hevc_mf`, `av1_mf`) and libvpx (`vp9_libvpx`) refuse in
their constructors; a recorder and the next probe child get it as
`ROOMLERD_ENCODER_CELLS_LEARNED`. A process that opens encoders outside these must
read the same list.

Also a config key (`encoder_cells_deny`, validated `name:chroma` or `none`),
pushable through remote config with `MANAGE_AGENTS` alone — it only ever
*removes* cells. The device reports `needs_restart`. Since 0.4.87 it gates **both**
chroma forms.

## 4 · A probe proves an OPEN; only a SESSION proves a CELL

`av1_vulkan` on an RTX 5090 opened, encoded, reported nothing wrong — and Chrome
painted garbage or stalled. FFmpeg's `av1_vulkan` emits temporal units **without**
the temporal-delimiter OBU every other AV1 encoder starts with.

`FfmpegEncoder::drain_packets` now prepends the delimiter to any AV1 packet
lacking one (`with_av1_temporal_delimiter`), for every AV1 backend — **necessary,
not sufficient**: the repaired build still showed correct keyframes and corrupt
inter frames, and the FFmpeg CLI's own `av1_vulkan` on the same GPU emits frames
dav1d cannot parse. That is an FFmpeg 9.0.1 / NVIDIA driver defect with no
roomler code in the loop, so `av1_vulkan:yuv420` is on the built-in denylist.

Diagnostic that attributed it in one run:
`encoder-smoke --name <x> --dump <file>` + the FFmpeg CLI's dav1d.

## 5 · Per-backend traps

⚠️ **Never flush an FFmpeg encoder that never took a frame.** `FfmpegEncoder::drop`
used to `send_eof` unconditionally, and a `hevc_vaapi` that had no picture issued
SEGVs inside `avcodec_send_frame(NULL)` on radeonsi — the probe child died on
every start and the host advertised no hardware, while `encoder-smoke` (ten
frames, then drop) passed. The flush runs only when `frame_count > 0`, reset per
rebuild.

| Backend | Trap |
|---|---|
| **NVENC** | 4:4:4 is planar `yuv444p`. `h264_nvenc` 4:4:4 needs `profile=high444p` — `rext` is HEVC-only and reads as "cannot". ⚠️ **The `nv-codec-headers` pin IS the driver floor**: FFmpeg refuses NVENC on a driver older than the SDK it was built against, and the host then advertises no NVENC cell. Linux pins 12.1 (530.41), Windows' vcpkg `ffnvcodec` 13.0 (570), and each Linux vendor job asserts the floor in the binary, so a pin bump moves that assert too. A pin at 13.1 cost every Linux host below driver 610 its NVENC (FR-122 P0c), and the dev box's newer driver could not show it. ⚠️ In a container, a working `nvidia-smi` proves nothing about NVENC: the NVIDIA runtime mounts `libnvidia-encode.so.1` and `libnvcuvid.so.1` only for the `video` driver capability (`docs/encoders.md`) |
| **QSV** | 4:4:4 is packed **VUYX**, never planar (this is why the FR-77 P1 `vp9_qsv` 4:4:4 open failed). Profile (`rext` / `profile1`) set explicitly in the `base` tier. `hw: true` only on the oneVPL build (`qsv_is_hardware_by_construction`) |
| **VAAPI** | Takes **hardware frames**; `encode/ffmpeg/vaapi.rs` holds the raw `ffmpeg_sys_next` calls. Device opens ONCE per process; each encoder gets a frame pool whose ref goes onto `hw_frames_ctx` **before** `open`. libva + libdrm are **bundled**, with **no `Depends`** — a `Depends: libva2` only holds where apt reaches a mirror, and the updater's offline `dpkg --install` replaces the binary *before* the dependency failure |
| **D3D12 / Vulkan** | `dlopen`'d by FFmpeg itself — the vendor jobs assert **no link directive / DT_NEEDED**, so a host without them loses cells, never the daemon. Set `bf=0` (both default to `bf=2`). `rc_mode` constants are **UPPERCASE** for d3d12va (`VBR`), lowercase for Vulkan (`vbr`) |
| **Media Foundation** | `ActivateObject`'s caller must end what it made, and a release does not: `HwMftCandidate`'s `Drop` calls `ShutdownObject`, or each open leaves 72 threads behind on a host with an AMD GPU. ⚠️ Candidates end when the cascade ends, **never between adapters**: until then `ActivateObject` returns the same instance, and a new one would change what wins (`docs/encoders.md`) |
| **VideoToolbox** | macOS ships **no FFmpeg at all**. Wiring it needs BOTH the `*_videotoolbox` names in the tables **and** a re-run of `vendor-ffmpeg-macos.yml` — restoring the feature alone re-ships a dyld crash on every end-user Mac |

⚠️ **The OPEN decides the device** (`open_on_some_device`: preferred first, then
the rest, winner remembered). Vulkan device 0 may be an iGPU with no
`VK_KHR_video_encode_queue`; "first device that opens" would strand the discrete
GPU's cells.

⚠️ **VP9 has NO 4:2:0 fallback on a rejected 4:4:4 open** (`new_vp9_adaptive`
returns `Err`): the viewer configured its decoder for profile 1, and a VP9 profile
mismatch is a blank canvas. The session dispatch probes the exact cell first and
runs libvpx when it cannot open. HEVC and H.264 fall back to 4:2:0 and report the
truth.

## 6 · Selection order

**CLI `--encoder` > env `ROOMLERD_ENCODER` > `encoder_preference` in config TOML >
`Auto`.** Values: `auto` | `hardware` (`hw`/`mf`) | `software` (`sw`/`openh264`).
`Auto` on Windows runs the MF H.264 probe-and-rollback cascade then falls back to
openh264; everywhere else it is openh264 only. `ROOMLERD_HW_AUTO=0` reverts to
openh264-first without a rebuild.

Legacy caps fields keep their exact pre-FR-77 meaning (first backend that opens),
and **`data-channel-vp9-444` stays on the wire forever**. Unknown cell names are
ignored, never an error. The viewer derives cells for old agents in
`ui/src/composables/videoCells.ts` — the one reading the picker and the admin
chips share.
