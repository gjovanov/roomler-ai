# Remote Control — Handover #3

> Third handover. #1 scoped the subsystem; #2 picked it up after the
> implementation sprint and drove it to live-verified on Win11 + openh264.
> This one hands off the **Windows Media Foundation hardware encoder**
> work (Phase 3) from a Linux dev box to a Win11 dev box so the remaining
> three commits can be validated against real NVENC + Intel QSV hardware.
>
> **Read order when resuming**: this file → `docs/remote-control.md` §17
> → the two Phase 3 commits already landed → "What to do next" below.

## tl;dr

- 0.1.25 is **live-verified** on Win11 + openh264 against `https://roomler.ai`.
  MF hardware encoder path is scaffolded but **opt-in** (`--encoder hardware`)
  because NVENC init fails without DXGI adapter matching and Intel QSV is
  async-only.
- Phase 3 was planned as 5 commits. Commits 1+2 landed on `master`
  (unpushed, 3 commits ahead). Commits 3+4+5 need Win11 hardware to validate.
- This session was paused before commit 3 on purpose: every remaining
  step changes behaviour in ways that can only be shaken out on your box
  (Intel UHD 630 + NVIDIA GTX 1650 + primary display 3840×2160).

## Commit chain (unpushed local commits on `master`)

```
61b0bca  feat(mf-encoder):    DXGI adapter enumeration + adapter-bound D3D11 device
3eb523b  refactor(mf-encoder): split into encode/mf/{mod.rs,sync_pipeline.rs}
f52eea4  docs:                document 0.1.25 encoder story + phase 3 scope
```

Prior (on origin/master):

```
264d17c  fix(encode):         demote MF backend from Auto cascade; default to openh264 on Windows
64b4b26  fix(mf-encoder):     stable SW MFT + optional D3D manager assist, ship 0.1.24
ac0d66e  fix(mf-encoder):     SET_D3D_MANAGER E_NOTIMPL is non-fatal
c64cfc9  feat(mf-encoder):    phase 2 — D3D11 device + IMFDXGIDeviceManager binding
cce5456  fix(mf-encoder):     swallow E_INVALIDARG on codec-api tuning knobs
```

First thing to do on Win11: `git push origin master` (or pull from your
Linux box and push from there) so the 3 local commits are on GitHub.

## Where we are

| Layer | State |
|---|---|
| openh264 path (default on Windows today) | ✅ live-verified, ships in 0.1.25 |
| MF-SW path (`--encoder hardware` falls through) | ⚠ works but overshoots bitrate ~5×, not recommended for production |
| MF-HW NVENC path | ❌ `ActivateObject` returns 0x8000FFFF without adapter matching |
| MF-HW Intel QSV path | ❌ async-only, ignores `MF_TRANSFORM_ASYNC_UNLOCK`; needs `IMFMediaEventGenerator` event loop |
| Phase 3 commit 1 (refactor) | ✅ landed |
| Phase 3 commit 2 (adapter enum) | ✅ landed, `#[allow(dead_code)]`, unused by cascade |
| Phase 3 commit 3 (probe-and-rollback) | ⏳ **your task** |
| Phase 3 commit 4 (async pipeline for QSV) | ⏳ **your task** |
| Phase 3 commit 5 (Auto → HW on Windows) | ⏳ **your task, only after 3+4 validate** |

## What to do next (in order)

### 1. Push the unpushed commits

```powershell
git status            # should show "3 commits ahead of origin/master"
git push origin master
```

### 2. Sanity-check the build on Win11

The `full-hw` feature pulls in MF + D3D11 + DXGI bindings from the
`windows` crate. First build takes ~5 min (openh264 compiled from C).

```powershell
cargo build -p roomler-agent --release --features full-hw
.\target\release\roomler-agent.exe --version    # expect 0.1.25
```

Run the existing smoke (SW openh264 baseline, should pass):

```powershell
$env:RUST_LOG = "roomler_agent=debug,info"
.\target\release\roomler-agent.exe encoder-smoke --encoder software
```

Run the MF-SW fallback smoke (probes current MF path without the new
cascade — baseline before commit 3 lands):

```powershell
.\target\release\roomler-agent.exe encoder-smoke --encoder hardware
```

Expected log lines on a healthy run:
- `mf-encoder: D3D11 device + DXGI manager created`
- `mf-encoder: activated MFT (sw phase 2)`
- `mf-encoder: pipeline ready`
- At least one `mf ProcessOutput produced bytes=...`
- Exit code 0

If any of those are missing, fix the baseline **before** starting commit 3.

### 3. Implement commit 3 — probe-and-rollback cascade

**Goal**: wire `adapter::enumerate_adapters()` + `adapter::create_d3d11_device_on()`
into the MFT activation path, and add a probe-frame step so each
adapter × MFT combination is verified before being accepted.

**Files**:
- New: `agents/roomler-agent/src/encode/mf/activate.rs` — split
  `activate_h264_encoder` out of `mod.rs` (currently `#[allow(dead_code)]`)
  and extend it into a `try_activate_and_probe` cascade.
- New: `agents/roomler-agent/src/encode/mf/probe.rs` — feed one synthetic
  NV12 frame at 480×270 (1/4 linear of 1080p), assert non-zero output
  within the existing 64-iteration drain cap.
- Modified: `agents/roomler-agent/src/encode/mf/sync_pipeline.rs` —
  `MfPipeline::new` takes the cascade result (pre-activated MFT + D3D
  device + DXGI manager) instead of building the SW MFT directly.
- Modified: `agents/roomler-agent/src/encode/mf/mod.rs` — drop the
  `#[allow(dead_code)]` on the adapter module; drop the monolithic
  `activate_h264_encoder` in favour of the cascade.

**Key types to add**:
```rust
// activate.rs
pub(super) struct HwMftCandidate { pub friendly_name: String, pub activate: IMFActivate }
pub(super) fn enumerate_hw_h264_mfts() -> Result<Vec<HwMftCandidate>>;

// probe.rs
pub(super) fn probe_pipeline(transform: &IMFTransform, codec_api: &ICodecAPI, width: u32, height: u32) -> Result<()>;

// thiserror enum in mod.rs or a new errors.rs
enum MfInitError { AsyncRequired, ProbeFailed(windows::core::Error), AllExhausted }
```

**Cascade logic** (inside `MfPipeline::new`, pseudocode):
```rust
for adapter in adapter::enumerate_adapters()? {
    let (device, manager) = adapter::create_d3d11_device_on(&adapter.adapter)?;
    for candidate in enumerate_hw_h264_mfts()? {
        match try_activate_and_probe(&candidate, &device, &manager, width, height) {
            Ok(pipeline) => return Ok(pipeline),
            Err(AsyncRequired) => { /* route to commit 4's async pipeline */ }
            Err(e) => { tracing::warn!(%e, "probe failed — next candidate"); continue; }
        }
    }
}
// All HW combinations exhausted — fall through to MS SW MFT (current code path).
```

**Probe frame**:
- Dims: 480×270 (even, minimal allocation).
- NV12 content: all-black Y plane + neutral chroma (0x80). Use
  `vec![0u8; w*h + w*h/2]` with the chroma half set to 0x80.
- Feed via `build_input_sample` with `frame_index = 0`.
- Call ProcessInput → drain loop. Timeout = existing 64 iterations.
- Success = at least one sample with `GetTotalLength() > 0`. Failure
  = all iterations return NEED_MORE_INPUT, or any HRESULT.

**Smoke command after implementation**:
```powershell
.\target\release\roomler-agent.exe encoder-smoke --encoder hardware
```

Expected on your NVIDIA GTX 1650:
- `mf-encoder: enumerated DXGI adapters count=2 adapters=["NVIDIA...", "Intel..."]`
- `mf-encoder: adapter-bound D3D11 device + manager created` (for NVIDIA)
- `mf-encoder: probing NVIDIA H.264 MFT`
- Either **pass** (probe outputs bytes) or `probe failed — next candidate`
  with the HRESULT logged.
- Eventually one candidate wins, or we fall through to MS SW MFT and
  exit 0.

**Failure modes to expect**:
- NVENC still returns 0x8000FFFF: the adapter isn't actually active.
  Optimus laptops put the dGPU to sleep until something creates a
  render target on it. Log it, roll back, next candidate.
- `DXGI_ERROR_UNSUPPORTED` on `D3D11CreateDevice` for the NVIDIA
  adapter: same Optimus issue. Already handled by the adapter.rs
  rollback note — the cascade just moves on.
- **If NVENC still fails after adapter-matching**: log the full
  driver version + HRESULT and check the user memory at
  `~/.claude/projects/-home-gjovanov-roomler-ai/memory/feedback_windows_mf_encoder_pitfalls.md`
  (on the Linux box) for any not-yet-recorded workaround.

### 4. Implement commit 4 — async pipeline for Intel QSV

**Goal**: handle async MFTs via `IMFMediaEventGenerator::BeginGetEvent`
+ `IMFAsyncCallback`. Intel QSV is the main target; some NVENC drivers
are also async.

**Files**:
- New: `agents/roomler-agent/src/encode/mf/async_pipeline.rs`

**Threading model**:
- The existing pinned worker (in `mod.rs::run_worker`) stays.
- `AsyncMfPipeline` owns an **event worker thread** that calls
  `IMFMediaEventGenerator::BeginGetEvent(&callback, None)`.
- The callback is a struct implementing `IMFAsyncCallback` via
  `#[implement(IMFAsyncCallback)]`. Its `Invoke` method receives
  `METransformNeedInput` / `METransformHaveOutput` events.
- Events are forwarded via `std::sync::mpsc` to the pinned worker,
  which calls `ProcessInput` / `ProcessOutput` on demand.

**Critical rules** (will silently break if violated):
- `IMFAsyncCallback::GetParameters` must return `S_OK` with flags = 0.
- On shutdown: `ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)` **before**
  stopping the event loop, else QSV leaks its D3D11 context.
- Callback COM object must outlive all pending `BeginGetEvent` calls.
  Use `Arc<EventSink>`; hand a clone to each `BeginGetEvent`.
- Never panic inside `Invoke` — it's an FFI boundary.

**Probe integration**: `try_activate_and_probe` in commit 3 should
detect `MF_TRANSFORM_ASYNC=1` and return `Err(MfInitError::AsyncRequired)`
so the cascade routes to the async path.

**Smoke expectation on your Intel UHD 630**:
- After commit 4, `encoder-smoke --encoder hardware` should exercise
  the async path on the Intel adapter.
- Success = async pump emits ≥1 `EncodedPacket` within 500 ms of the
  first probe frame.
- Failure = log the exact `METransformError` code and fall back to
  the next candidate.

### 5. Implement commit 5 — re-promote Auto → HW on Windows

**Goal**: make `--encoder auto` (the default) try MF-HW first on
Windows and fall back to openh264.

**Files modified**:
- `agents/roomler-agent/src/encode/mod.rs` — `open_default()` Auto
  branch: on Windows with `mf-encoder`, try `mf::MfEncoder::new` first,
  fall through to openh264.
- `CLAUDE.md` — update "Encoder selection (Windows)" section: Auto
  path is now `MF-HW (probe-rollback through adapter/MFT cascade)
  → openh264 → Noop`. Close the `[HIGH] [2026-04-18]` Known Issue.
- `docs/remote-control.md` §17 — mark Phase 3 complete.

**Escape hatch**: env var `ROOMLER_AGENT_HW_AUTO=0` forces Auto back
to openh264-first. Document in CLAUDE.md. If post-landing bugs appear
in the field, this is a one-line patch to flip the default.

**Smoke**: `encoder-smoke --encoder auto` on your Win11 box should
now pick `mf-h264`. Full end-to-end test: `roomler-agent run --encoder auto`
and connect from `https://roomler.ai/remote` — video should flow via
the HW encoder.

## Architecture cheat-sheet

```
agents/roomler-agent/src/encode/
├── mod.rs                     # VideoEncoder trait, EncoderPreference,
│                              # open_default() cascade
├── color.rs                   # BGRA → NV12 software converter
├── openh264_backend.rs        # software fallback (stable, default today)
└── mf/                        # Windows Media Foundation encoder (feature-gated)
    ├── mod.rs                 # MfEncoder handle, COM/MF lifecycle,
    │                          # create_d3d11_device_and_manager (default path),
    │                          # activate_h264_encoder (dead code, commit 3 replaces)
    ├── adapter.rs             # ← Phase 3 commit 2: DXGI enum + per-adapter device
    ├── sync_pipeline.rs       # synchronous MFT pump (current path)
    ├── activate.rs            # ← Phase 3 commit 3: HW MFT cascade + probe routing
    ├── probe.rs               # ← Phase 3 commit 3: single-frame probe harness
    └── async_pipeline.rs      # ← Phase 3 commit 4: event-driven pump for QSV
```

## Gotchas you'll hit on Win11

Every one of these cost a round trip during 0.1.23 → 0.1.25. Do not
relearn them.

1. **`CLSID_MSH264EncoderMFT`**, not `CLSID_CMSH264EncoderMFT`. Docs
   disagree with the `windows` crate.
2. **`GetOutputStreamInfo` / `GetTotalLength`** return `Result<T>`,
   not HR + out-param. The crate already unwraps HRESULT.
3. **`MFTEnumEx` needs `MFT_ENUM_FLAG(raw_i32)`** — bitflag composition
   by hand.
4. **`MFT_MESSAGE_SET_D3D_MANAGER` returning E_NOTIMPL is normal on
   the SW MFT** — keep going, don't bail. Only HW MFTs care.
5. **`set_codec_u32` must swallow both E_FAIL AND E_INVALIDARG.**
   `LowDelayVBR` returns E_INVALIDARG on SW MFT; treating it as fatal
   bricks init.
6. **`D3D_DRIVER_TYPE_UNKNOWN` required when passing a non-null
   adapter** to `D3D11CreateDevice`. Passing `HARDWARE` returns
   E_INVALIDARG. (Already baked into `adapter::create_d3d11_device_on`.)
7. **Without a DXGI device manager** the SW MFT silently accepts
   input but produces zero output. You need D3D11 device (BGRA + VIDEO
   support, multithread-protected) → `MFCreateDXGIDeviceManager` →
   `ResetDevice` → `SET_D3D_MANAGER`.
8. **NVENC `ActivateObject` returns 0x8000FFFF** on hybrid laptops
   without adapter matching — this is the whole point of commit 3.
9. **Intel QSV MFT ignores `MF_TRANSFORM_ASYNC_UNLOCK=TRUE`** — the
   sync drain loop will hit `NEED_MORE_INPUT` forever. This is what
   commit 4 solves.
10. **SW MFT bitrate control lies**: rejects `LowDelayVBR`, falls to
    quality-VBR, overshoots target bitrate ~5×. 20%+ packet loss on
    typical uplinks. Never let the Auto cascade land here.
11. **Probe-and-rollback pattern is non-negotiable**: NVENC/QSV report
    OK on `ActivateObject` but fail on first real frame. Feed a test
    frame before declaring the MFT healthy.
12. **Keep `_d3d_device` and `_d3d_manager` as fields on the pipeline
    struct** — the MFT holds weak refs and crashes if either drops
    early. Underscore prefix documents the lifetime intent.

## Test matrix

| Scope | Command | Runs on |
|---|---|---|
| Default unit tests | `cargo test -p roomler-agent --lib` | any |
| MF adapter unit tests (commit 2) | `cargo test -p roomler-agent --lib --features mf-encoder` | Windows only (cfg-gated) |
| SW MFT smoke baseline | `roomler-agent.exe encoder-smoke --encoder hardware` | Windows |
| openh264 smoke baseline | `roomler-agent.exe encoder-smoke --encoder software` | Windows |
| Full pipeline smoke | `roomler-agent.exe run --encoder auto` + browser | Windows + browser |
| Release CI | `.github/workflows/release-agent.yml` on tag push | GitHub Actions Windows |

## Reference material

- **Phase 3 plan (detailed)**: generated by the `planner` agent,
  stored in conversation history. Five-commit plan with file paths,
  new type signatures, verification steps, risks per commit. Ask the
  next session to re-run the planner with the same prompt if you need
  the full text.
- **Architecture + design**: `docs/remote-control.md` (17 sections).
- **Live-debugging lessons**: on the Linux box at
  `~/.claude/projects/-home-gjovanov-roomler-ai/memory/feedback_remote_control_debugging.md`
  (14 gotcha-class lessons from the 0.1.23 → 0.1.25 sprint).
- **MF-specific pitfalls**: on the Linux box at
  `~/.claude/projects/-home-gjovanov-roomler-ai/memory/feedback_windows_mf_encoder_pitfalls.md`
  — content duplicated in "Gotchas you'll hit on Win11" above, so you
  don't strictly need it, but useful cross-reference.
- **User hardware profile**: Win11, Intel UHD 630 + NVIDIA GTX 1650,
  primary display 3840×2160. Saved in `user_controlled_host.md` on
  the Linux box.
- **Production endpoints**: API + browser at `https://roomler.ai`
  (HTTPS, do not use `http://` — 308 strips POST body). WS signalling
  at `wss://roomler.ai/ws?token=<agent-jwt>&role=agent`.

## Known issues still open (not Phase 3)

Copied from `CLAUDE.md` for quick reference:

- `[HIGH]` JWT default secret is "change-me-in-production" — must be
  overridden in prod.
- `[MEDIUM]` Remote-control: clipboard + file-transfer data channels
  accepted on both sides but still log-only.
- `[MEDIUM]` Remote-control: consent auto-granted on agent (no tray
  UI yet).
- `[LOW]` Deployment strategy is Recreate (no rolling updates).
- `[LOW]` No git hooks configured.
- `[LOW]` Remote-control: encoder bitrate is not TWCC/REMB-adaptive
  mid-stream.
- `[LOW]` Remote-control: agent captures primary display only.

## Definition of done for Phase 3

- [ ] Commit 3 lands: probe-and-rollback cascade; `encoder-smoke
  --encoder hardware` on your box logs adapter enum + probe pass/fail
  lines, exits 0 on all code paths (even when NVENC still fails).
- [ ] Commit 4 lands: async pipeline; Intel QSV probe either passes
  or fails cleanly with a specific HRESULT logged; no hangs.
- [ ] Commit 5 lands: Auto → HW on Windows; default `run` picks
  `mf-h264`; env var `ROOMLER_AGENT_HW_AUTO=0` reverts to openh264;
  full remote-control session at `https://roomler.ai` shows HW-encoded
  video flowing.
- [ ] `CLAUDE.md` Known Issues "Windows MF hardware encoder" flips to
  `Status: FIXED` with the env-var escape hatch noted.
- [ ] `docs/remote-control.md` §17 "Phase 3" marked complete.
- [ ] A new release tag (0.1.26 or 0.2.0) triggers
  `release-agent.yml`, which should now probe HW on the Windows
  runner without regressing.
