# FR-85 — HQ screen recording: a local or remote screen to a file, then cut, speed up and score it

**Issue:** [#1634](https://github.com/gjovanov/roomler-ai/issues/1634) · **Status:** **in progress** — P0's FFmpeg half measured (field log); P1a (the recorder core), P1b (the hardware recording profile) and P2a (the local verbs: LocalAPI, console-user gate, `record_dir`, `roomler record`) built behind the `recording` feature, which no release build carries yet · **Numbering:** claimed as FR-84 and renumbered before landing — [#1633](https://github.com/gjovanov/roomler-ai/issues/1633)'s FR-84 reached master first (issues 68 s apart), and the ledger arbitrates · **Anchors:** master `d5db02c0f` · **Glossary:** [`CONTEXT.md`](../../CONTEXT.md) · **Related:** [FR-27](FR-27-consent-surfaces-and-desktop-companion.md) · [FR-44](FR-44-shared-pipeline-multi-viewer.md) · [FR-70](FR-70-media-pipeline-leaves-the-runtime.md) · [FR-77](FR-77-encoder-chroma-matrix.md) · [FR-80](FR-80-a-session-with-no-pixels-says-why.md) · [FR-24](FR-24-licensing-split.md) · [FR-61](FR-61-vmtest-matrix.md) · [`docs/remote-control.md` §11.3](../remote-control.md)

## Goal

Record a screen in high quality to a file, through roomler, in two situations:

1. **Local** — the person at a device running roomlerd + roomler-desktop records their own screen.
2. **Remote** — a controller in the browser viewer records the screen of the device they are controlling. The file is
   written **on that device**, and the controller downloads it on demand over the session's P2P channel.

Recordings land in a **default folder the user can override from roomler-desktop**. A roomler-desktop editor then
**cuts** segments, **speeds up** segments and mixes in **background music**, exporting a new HQ file. The feature states
what it costs in installer size and ships with integration and e2e tests.

## Why — what exists and what does not

- **The permission was designed and never built.** `Permissions::RECORD` is defined
  (`crates/remote_control/src/permissions.rs:21`) and already in `requires_consent_prompt()`, but nothing in `agents/`,
  `crates/modules/` or `ui/src/` uses it. `RemoteSession.recording_url` is always `None`. `docs/remote-control.md`
  §11.3 (`:679`) promises a banner that cannot be dismissed and a red tray dot while recording; neither exists.
- ⚠️ **Any tab can already request RECORD.** `resolve_session_authz` (`crates/modules/remote/src/controller.rs:375`)
  never reads the requested permissions, and `Hub::create_session` narrows only INPUT
  (`crates/modules/fleet/src/hub.rs:811-834`). Harmless today because no agent honours the bit. It stops being harmless
  the day one does, so the strip ships before the agent learns the bit (P3).
- **Recording what the viewer receives would not be HQ.** The live stream is rate-controlled to the network
  (FR-62/63/79) and its capture is capped by the live plan (`agents/roomlerd/src/peer.rs` `set_output_cap`), so the
  viewer's copy is transport quality. HQ means encoding at the source, in a pipeline of its own.
- **No muxer exists anywhere.** The agent emits raw NAL units over a DataChannel. Vendored FFmpeg n9.0.1 is
  `--disable-everything` plus hardware encoders only: static in `roomlerd.exe`, shared `libavcodec.so` in the .deb,
  dylibs in `roomlerd.app`. No decoders, demuxers, muxers, filters or swscale. **Linux arm64 has no FFmpeg at all**
  (openh264 + libvpx).
- **The parts that do exist:** the capture cascade `capture::open_default` (`agents/roomlerd/src/capture/mod.rs:665`)
  and a synthetic source with a decodable 64-bit counter strip (`capture/synthetic_backend.rs:41`); `EncoderThread`
  (`encode/thread.rs:118`); the cells, denylist and probe cache; the child-process probe pattern (`encode/caps.rs`); the
  uid drop used by the portal helper (`capture/portal/mod.rs`); cpal loopback audio + audiopus Opus (`audio/`); the
  consent chain and `PromptKind` (`consent.rs:157`); the config surface (`crates/agent-core/src/config_surface.rs:1023`)
  and LocalAPI `ConfigSet`; the capture-excluded Windows indicator (`indicator/win.rs`); the updater's
  `decide_defer` (`updater.rs:119`); the files-DC chunk pump (`peer.rs:10344`); `.roomler-partial` staging (`files.rs`).

## Decisions taken with the operator (2026-09-25)

| Question | Decision |
|---|---|
| Where does a **remote** recording end up? | **On the controlled device**, in its recording folder. The controller downloads it on demand over P2P. The device owner always keeps the copy. |
| Which audio does a recording capture? | **System audio and microphone, both default OFF.** |
| Can a remote controller turn on the host's microphone? | **Never** (this spec). The microphone toggle exists only on local surfaces. |

## Key design

### 1. One recorder, two launchers

- **`roomlerd record`** — a new subcommand of the same binary, spawned by the worker and driven by JSON lines on
  stdin/stdout, the probe-child pattern. Parent death means stdin EOF, which means finalize and exit; EPIPE on stdout
  during finalize is tolerated. A driver fault in the recorder never takes the daemon or a live session down. Handles are
  not inherited, so a respawned worker cannot trip its own single-instance lock.
- **Identity rule.** Whenever a console user exists, the recorder runs **as that user at normal integrity** — never
  SYSTEM, never root, never elevated. On Windows the worker holds the interactive admin's *elevated* linked token by
  default (`ROOMLERD_ELEVATE_WORKER`, `agents/roomlerd/src/win_service/supervisor.rs:325`), so the recorder gets a
  restricted medium-integrity token; a SystemContext worker spawns it with the console user's token; a Linux root daemon
  drops to the console user the way the portal helper does. Only a host with **no console user** (headless, an Xvfb
  virtual desktop, nobody logged in) records as the daemon's identity.
- **Its own capturer**, never a tap on the live pump: native resolution whatever the live plan says, one code path for
  local and remote. A second capturer is viable on every backend — WGC takes a second session; DXGI spends one of its
  ~4 duplication seats; X11, CGDisplayStream and DRM are unaffected. The cursor comes from `ROOMLERD_WGC_CURSOR=1` on WGC
  and is composited from `capture/cursor.rs` everywhere else. ⚠️ On a Wayland portal a second session may raise a second
  dialog: P0 measures whether the persisted restore token suppresses it, and if not, portal hosts tee the worker's stream
  (P1b).
- **CFR pacer**, 30 fps (60 opt-in). Frames are stamped on the recorder's own `Instant` at receipt and PTS = tick / fps,
  repeating the last frame on `Ok(None)`. ⚠️ Never `Frame.monotonic_us`: its origin differs per backend, and on the portal
  it is wall-clock. A lock screen or `ACCESS_LOST` re-acquires and inserts a black gap; it never aborts. A fast user
  switch terminates the worker, so the recording ends `session_changed` — a recording never follows a user switch.
- **Recording encoder profile.** A new `RateMode { ConstantQuality { q }, Vbr { … } }` and an explicit GOP are threaded
  through `build_encoder` (`encode/ffmpeg/encoder.rs:1422`) and `encoder_options` (`:240`); today both force
  maxrate/bufsize and a 64 800-frame GOP (`:51`).

  | Backend | Recording options |
  |---|---|
  | NVENC | `rc=vbr cq=N b:v=0 preset=p6 tune=hq` |
  | QSV | `global_quality=N b:v=0`, no maxrate, `low_power=0` |
  | AMF | `rc=cqp qp_i=qp_p=N` |
  | VAAPI / D3D12 / Vulkan | CQP, `qp=N` |
  | VideoToolbox | high ABR (≈ 0.3 bits per pixel) |
  | openh264 | `RateControlMode::Quality` or fixed QP |

  ⚠️ **As built in P1b** the recorder does NOT use new per-backend dicts yet. `FfmpegEncoder::new_recording` reuses
  the live option sets, which every fleet GPU has already opened, and changes only three things: `cq` 19, a recording
  ceiling instead of the network's, and a real GOP, plus counter-based PTS. A new private-option dict is a new way
  for a driver to refuse an open, and nothing here has measured one yet. The table above stays the target, and each row
  lands only when a field measurement on that vendor's hardware shows it opens and beats the live dict.

  GOP 2 s and `bf=0` (so PTS = DTS). The **live** option sets stay byte-identical, locked by a test. The recorder uses the
  same cascade, **denylist** and probe cache as live — the child receives the config-fallback denylist explicitly, or
  `encoder_cells_deny` would be silently ignored at this entry point. Default **H.264 4:2:0** (plays everywhere, and
  previews in WebView2); HEVC and 4:4:4 are opt-in.
- **Audio**, both default OFF. System audio is the existing cpal loopback (WASAPI on Windows, the monitor source on
  Linux; macOS has none — ScreenCaptureKit audio needs macOS 13 and the bundle targets 12). The microphone is a new cpal
  input stream. One `Instant` is the master clock; audio PTS come from accumulated samples, and a drift corrector fills
  silence or drops once the error passes 20 ms (which also covers WASAPI's gaps while the endpoint is idle). `rubato`
  resamples, and the sources are summed with a soft clip. Encoded as **Opus** (audiopus) in P1 and **AAC** after the P4
  FFmpeg bump; arm64 keeps Opus. A microphone failure is named, never silent: `mic_unavailable{privacy}` (the Windows
  "let desktop apps use the microphone" switch) or `{permission}` (macOS TCC), and the UI offers to record without it.
- **Container.** `mp4-atom` (MIT/Apache-2.0, allowed by `deny.toml`) writes **fragmented MP4** with keyframe-aligned
  fragments of about 2 s, so a crash loses at most 2 s. On stop the file is **remuxed to a progressive MP4 with
  `moov` first** (no re-encode) so QuickTime, Movies & TV and editors open it. A boot reconciler finalizes orphaned
  partials as `interrupted`. A sidecar `<name>.roomler.json` records the initiator (local user, or the controller's user
  id and name), device, encoder, dimensions, fps, audio sources, events (locks, gaps, dropped frames) and the stop
  reason. Guards: refuse below 2 GiB free, stop below 1 GiB, 240-minute maximum, one active recording per display.

### 2. The folder

- **Default**, resolved per user and shown in the UI with its reason: Windows `FOLDERID_Videos\Roomler` **only if** it
  is on a local fixed volume, not under OneDrive or another sync root, not a redirected share, and passes a real
  create + write + delete probe — Defender Controlled Folder Access lets `metadata()` succeed and then blocks the write.
  Otherwise `%USERPROFILE%\Roomler Recordings`. macOS `~/Movies/Roomler`; Linux `$(xdg-user-dir VIDEOS)/Roomler`.
- **Override**: `record_dir`, a device-owned config key and the surface's first path-valued key. The validator in
  `config_surface.rs:1023` refuses relative paths, `~`, UNC, `\\?\` and device paths, and reparse components.
  roomler-desktop sets it through `ConfigSet` with a native folder picker — a Rust command over
  `tauri_plugin_dialog::DialogExt`, because the webview has no dialog capability today — plus "Reset to default".
- **Write rule.** The recorder writes into the folder only when it runs as that folder's user. It stages in
  `<dir>/.roomler-partial/`, opened by handle (`FILE_FLAG_OPEN_REPARSE_POINT`, `O_NOFOLLOW | O_EXCL`), refuses reparse
  components, never calls `create_dir_all` on another identity's path, and renames on finish. Otherwise it stages in its
  **own data dir** and roomler-desktop — the same uid — renames or copies the file into the folder; that is the Linux
  user-unit case (`ProtectHome=read-only` in `agents/roomlerd/packaging/linux/roomler.service`). A folder that refuses
  both processes (Controlled Folder Access blocks roomlerd *and* roomler-desktop) keeps the file in the data dir with
  `reason=cfa`, and the UI names the fix. A host with no console user keeps recordings in the daemon's dir with an
  **explicit DACL** (SYSTEM + Administrators — `appdirs.rs` sets none today, so Users could read) or mode 0700 as root.
  Nothing is streamed over LocalAPI: it is one JSON line per response.

### 3. Local recording

- A **Recordings** view in roomler-desktop — the sixth entry in `VIEWS` (`agents/roomler-desktop/src/front/app.js:90`):
  Start/Stop with a timer and size; start options (system audio, microphone, fps, quality, codec, cursor), unchecked by
  default and remembered; the list (name, date, duration, size, origin, pending delivery) with Play, Edit, Show in
  folder and Delete; settings for the folder, "Allow remote recording" and "Include computer audio in remote
  recordings". The tray gets Start/Stop and a red REC dot.
- LocalAPI verbs (`crates/localapi/src/lib.rs:948`, the exhaustive `handle()` and `serve_connection`): `RecordStart`,
  `RecordStop`, `RecordStatus`, `RecordingsList`, `RecordingDelete`.
  ⚠️ **They need a peer check the pipe does not have.** The pipe's DACL admits every Interactive User, RDP sessions
  included (`lib.rs:1649`), and there is no peer-credential check, so a guest in an RDP session could start a local
  recording of the console user's desktop. The new verbs, and `ConfigSet` of any `record_*` key, require
  `GetNamedPipeClientProcessId` → `ProcessIdToSessionId` to equal the worker's session (`SO_PEERCRED` on unix).
- CLI: `roomler record start | stop | status | ls`.
- The updater counts an active recording in `decide_defer` (at most 7 × 1 h). A pushed `rc:agent.update` bypasses that
  gate, and the MSI's Restart Manager kills the child; fragments plus the boot reconciler turn that into a clean
  `interrupted` file, not a loss.
- **As built in P2a** (`docs/recording.md` §6):
  - The gate compares the client's session with the **active console session**
    (`WTSGetActiveConsoleSessionId`), which is the worker's session whenever the worker serves one. On unix the peer's
    uid (tokio `peer_cred`) must be the daemon's, or root. An unidentified peer fails closed. The gate sits in the
    listener (`serve_connection_as`), before any handler runs.
  - The daemon side is `recording/manager.rs` (`RecordingManager`). It spawns `roomlerd record` with the config
    fallbacks in its environment, so the encoder denylist binds the child. `start` answers on `started`/`refused`,
    `stop` once the file is final, and a child that exits silently ends as `recorder_exited`. The CLI gained `rm`.
  - **Until P1e, a SYSTEM/root daemon refuses a local recording** (`recording/identity.rs`). The child would inherit
    SYSTEM/root and save into the service account's own profile, where the person who pressed Record cannot see it.
  - **As built in P2b** (`docs/recording.md` §7): the view and the tray only ask the daemon. Two additions came out of
    building them. First, `RecordingState.available` / `unavailable_reason`, so a client greys Start out ahead of time
    instead of offering one that can only fail. Second, `cmd_config_set` no longer falls back to writing the file when
    the daemon *answered* a `record_*` change: that would have reported a success the console gate refused. The
    view's settings are the folder only; "Allow remote recording" and the audio toggles arrive with P3 and P1c.
  - The "boot reconciler" became **reconcile at every recorder start**. It runs beside the new recording so a big remux
    cannot delay `started`, and only for partials whose OS file lock (`<name>.partial.lock`, `PartialLock`) it can take.
    Without that lock a second recorder in the same folder would finalize, or delete, a live partial; the test that says
    so is red with the lock disabled.

### 4. Remote recording — the gates, in order

Each refusal carries a named reason on the wire and in the UI.

1. **Server authorization.** A new role bit `RECORD_REMOTE_SCREEN` (bit 31: `role.rs` `NAMED`, an `ALL` bump, in no
   managed row below ADMINISTRATOR; the UI mirror is `2 ** 31`, because `1 << 31` is negative in JavaScript).
   `resolve_session_authz` computes `SessionAuthz.may_record` — owner, ADMINISTRATOR or the role bit — and it is
   **false whenever `override_reason` is set**. Break-glass forces `ConsentMode::Auto` (`controller.rs:453`); allowed to
   record, it would be covert monitoring, which §11.4 rules out.
2. **Hub strip.** RECORD survives `create_session` only if `may_record` and the agent advertises
   `AgentCaps.record ∋ "remote"` — a new typed capability list in the `RpcCap` style (variant → `wire()` → `ALL`,
   matched by equality). The agent advertises it only while the device gate is on, and an old agent never does, so
   RECORD never reaches one.
3. **Device gate.** `record_remote_enabled` and `record_remote_audio`, both default **OFF**, device-owned and
   **structurally absent from `DesiredConfig`** — a `record_` twin of `no_relay_key_is_server_pushable_via_desired_config`
   (`crates/remote_control/src/models.rs:3938`). Capabilities are read once at hello, so ON takes effect at the next
   connect; **OFF is immediate** — new starts are refused and a running remote recording stops.
4. **A `record` DataChannel**, attached only when the grant holds RECORD — the same attach-time gate as `files` and
   `input`. The control channel carries no permission context, so it cannot host this. Messages:
   `rc:record.start {id, audio?}` → `rc:record.state {id, state, reason?, name?, bytes?, duration_ms?}`, with state in
   `pending_consent | recording | stopped | refused | failed`; `rc:record.stop`; `rc:record.list`;
   `rc:record.get {name, offset}` (binary chunks, resumable). The microphone is never a remote option.
5. **Just-in-time consent.** A host in prompt mode gets `PromptKind::Record` — "<controller> wants to record your screen
   [and computer audio]" — through the `PromptSurface` chain, with a **fresh ObjectId**, never the session id or a suffix
   of it (the `ssh` / `ssh-consent` lesson), `strictest_of`, one pending prompt per session and a cooldown after a
   deny. The connect prompt says plainly that the controller may ask to record. An auto-grant host skips the prompt:
   its owner opted in at gate 3.
6. **The indicator is up before the first frame.** On an attended host (a real console user): a banner that cannot be
   dismissed, with **Stop**, and a red tray dot (§11.3) — the native indicator on Windows (capture-excluded), the
   companion elsewhere. **No surface means `refused{no_indicator_surface}`.** An unattended host (no console user, or a
   virtual desktop) may record under gate 3. On X11 and macOS the banner is not capture-excluded and appears in the
   recording; that is documented, not hidden.
7. **Lifetime.** Stops on the controller's Stop, the host's Stop, the gate going OFF, or a disk/duration guard. A
   dropped session gets 60 s in which the same controller *user* re-attaches under a new session id: ownership is
   (controller user id, device), kept in the sidecar, because the reconnect ladder mints new session ids.
8. **Download.** `rc:record.list` and `rc:record.get` serve only recordings whose sidecar names this controller's user
   id, confined to the recording dirs — the name has no separators or `..`, the file is opened without following links,
   must be a regular file, and its final path is checked. Resumable by offset, so a relay flap does not restart 3 GB
   from zero; no 2 GiB cap. The viewer writes through `showSaveFilePicker` streams (Chrome, Edge); Firefox and Safari
   fall back to the capped Blob path with a clear message.
9. **Audit — the decision and the claim, kept apart as SSH keeps them.** The server's decision stays in `remote_audit`
   (the grant, with the strip reason). The host's claims — prompt outcome, started, stopped {bytes, duration, reason},
   downloaded {bytes} — go to a new `recording_activity` collection through a new `ClientMsg::RecordingActivity`
   (a `namespace()` arm owned by `remote`, the owners table, a TTL index in the module's index plan, the composition
   baseline re-recorded with `COMPOSITION_UPDATE=1` and a commit message that says why), checked against the live
   session's grant. Never content.

### 5. Editor and export

- An **Edit** view in roomler-desktop, plain HTML/JS like the rest of `src/front` (107 KB today): a `<video>` preview
  through the Tauri asset protocol, scoped at runtime to the recording folders (plus `media-src`/`img-src` in the CSP,
  `tauri.conf.json`), falling back to a thumbnail strip and a frame at the playhead when the webview cannot play the codec
  (WebKitGTK without GStreamer plugins, WebView2 without HEVC); a timeline with thumbnails; split at the playhead; per
  segment **Cut** or **Speed** 1.5 / 2 / 4 / 8 / 16×; **music** — file picker, volume, fade in and out, start offset,
  loop — and the original audio's volume; Export with a name, quality, progress and cancel. Edits are non-destructive,
  saved as `<name>.edit.json`.
- The engine is `roomlerd media probe | thumbs | frame | export --edl <file>`, spawned by a Rust command in
  roomler-desktop that validates every path, running as the user; it refuses to run elevated. **Untrusted media is never
  parsed inside the daemon.** `mp4-atom` demuxes; FFmpeg's software `h264`/`hevc` decoders decode after P4, and the
  openh264 decoder handles H.264 on arm64 and in the default CI lane. Frames are selected **by PTS** — a cut drops, a
  speed-up of k samples to the output's constant frame rate, and a frame-accurate cut decodes from the preceding keyframe
  (at most 60 frames at a 2 s GOP). The recording encoder profile re-encodes, and the output is a progressive MP4 with
  `moov` first. Audio: `symphonia` (MPL-2.0; mp3, AAC-LC, FLAC, Vorbis, WAV) decodes music, audiopus decodes Opus,
  `rubato` resamples, and gain, fades, loop and mix are plain Rust. The original audio is muted inside sped-up segments.
  AAC out (Opus on arm64). **No libavfilter, and no FFmpeg muxers or demuxers.**

### 6. How this squares with "session content is never recorded"

The SSH pillar never records session content, because a recording ships what the operator typed off the host. A screen
recording is content by definition, so it is bounded the same way: the bytes never leave the device except through an
explicit, permission-gated P2P download; recording is visible on the host while it happens; the device gate is OFF by
default; break-glass can never record; and the server stores only the fact. `recording_url` stays `None`, and no upload
path will exist.

## Installation size — the evaluation

Measured on the 0.4.101 release (see the field log). The Windows MSI guard divides by PowerShell's `1MB`, so its units
are MiB: the MSI is **15.36 MiB against an 18 MiB hard ceiling** (`.github/workflows/release-agent.yml:1704`), 2.64 MiB of
headroom. Its pinned 13.13 baseline is stale, so the +2 warn band fires already, before this FR adds a byte.

**What keeps it small:** no ffmpeg CLI; no new DLL, `.so` or `.dylib`; no libavfilter; no FFmpeg muxers or demuxers
(`mp4-atom` does MP4); no swresample (`rubato`); music decoding in pure-Rust `symphonia`, which is also the safer parser
for untrusted files; and the companion stays FFmpeg-free, because the export runs in the `roomlerd` binary the device
already has.

| Adds | Ships in | Uncompressed | In the installer |
|---|---|---|---|
| FFmpeg `h264` + `hevc` software decoders and the `aac` encoder (one vendored bump, P4) — **measured in P0** | `roomlerd.exe` (static) · `libavcodec.so` · `.dylib` | **+2.79 MiB** linked (Windows) · **+2.04 MiB** (`.so`) · **+1.85 MiB** (`.dylib`) | **+0.82 MiB** (MSI, deflate) · **+0.47 MiB** (.deb, xz) · **+0.42–0.69 MiB** (.pkg) |
| `symphonia` (trimmed features) + `rubato` | roomlerd, all platforms | +0.75–1.05 MB (est.) | +0.3 MB (est.) |
| recorder, export engine, `mp4-atom`, LocalAPI and CLI verbs | roomlerd, all platforms | +0.5–0.8 MB (est.) | +0.2 MB (est.) |
| the openh264 **decoder** (already compiled by `openh264-sys2`, now kept by the linker) | roomlerd | +0.4 MB (est.) | +0.15 MB (est.) |
| microphone capture (cpal is already linked) | roomlerd | ~0 | ~0 |
| Recordings and Edit views, the folder-picker command | roomler-desktop | +0.05–0.1 MB (est.) | < 0.05 MB |
| **Total** | | **+4–5 MB** | **≈ +1.1–1.5 MB per installer (≈ +7–10 %)** |

- **Windows MSI ≈ 16.8 MiB** (15.36 today + 0.82 measured + ~0.65 estimated), under the 18 MiB ceiling with about
  1.2 MiB to spare. P4 re-pins the baseline **from the measured release run, never from this estimate** — the guard's
  own rule — and says what grew.
- Linux x64 .deb ≈ +0.8–1.1 MB (6–8 %) · macOS .pkg ≈ +0.8–1.3 MB (4–6 %) · **Linux arm64 .deb ≈ +0.3–0.5 MB** (no
  FFmpeg: it records openh264 + Opus and edits H.264 through the openh264 decoder) · roomler-desktop < 0.1 MB.
- The P0 FFmpeg numbers came from a branch that is never merged (`fr84-p0-measure`, named before the renumber): the
  three vendor builds with the P4 flags, uploaded as Actions artifacts only, and — for Windows, where FFmpeg links
  statically and archive size means nothing — the same registry-pulling probe linked with MSVC against both trees.
- Levers if P0 measures high: H.264-only editing (drop the `hevc` decoder, ≈ −0.4 MB) · MP3/WAV/M4A-only music (≈ −0.1 MB).
- Not installation size, but why the disk guard exists: HQ 1080p30 screen content is about 1–3 GB per hour, 4K60 about
  4–10 GB per hour.

## Rejected designs

- **Record what the viewer receives.** Transport quality: rate-controlled, capped by the live plan, degraded by loss.
- **Tap the live pump's frames** (`peer.rs`, after the capture). It gets the viewer's capped rung, needs a live session,
  and couples a new consumer to the most-tuned path in the product.
- **libavfilter for the export.** +0.6–1 MB and a filter-argument injection surface, for things that are a few lines of
  Rust.
- **An ffmpeg CLI** (+20–80 MB) · **ffmpeg.wasm** (~30 MB download, slow) · **a WebCodecs editor in the webview** (≈ 0 MB,
  but WebKitGTK cannot export reliably, so Linux would have no editor).
- **Streaming the file to the companion over LocalAPI.** LocalAPI is newline-delimited JSON, one line per response; a
  multi-GB stream is a protocol change on a leaf crate with three clients, and a same-uid `rename(2)` does the job.
- **Choosing the staging path by "is the process elevated".** Wrong on the most common admin laptop: the Windows worker
  of a UAC-split administrator holds the elevated linked token by default, so every such host would take the spool
  path; and an elevated writer in a user-writable folder is a medium-to-high integrity write primitive. The rule is
  identity — the recorder *is* the user.
- **Just-in-time consent "only when the mode is Prompt", with no override check.** Break-glass forces `Auto`, so an
  administrator's recording would have been silent.
- **Carrying recording on the control channel.** It has no permission context; `files` and `input` gate at attach time.
- **Downloading through the files channel, keyed by session id.** That channel exists only when FILES is granted, and
  the reconnect ladder mints a new session id on every flap.
- **Videos as an unconditional default.** OneDrive Known Folder Move would sync gigabytes by default; Controlled Folder
  Access blocks the write after `metadata()` says it is fine.

## Phases

| # | Phase | Kill switch | Status |
|---|---|---|---|
| **P0** | Measure: dispatch the FFmpeg vendor workflows with the P4 flags under a **new, unreleased asset name** (never overwrite the production asset) → library sizes; one rehearsal release run → both MSI sizes; link a stub pulling `mp4-atom` + `symphonia` + `rubato` with and without (how `ssh-server` was measured, `agents/roomlerd/Cargo.toml` ~:629); Windows: a second WGC session and the DXGI seat budget beside two viewers; macOS: TCC for a `roomlerd record` child, and the microphone prompt; Wayland: a silent second portal session through the restore token? | nothing ships | **FFmpeg half measured** on all three platforms (field log). Open: the Rust-side link delta, the Windows seat/second-session smoke, macOS TCC for the child, the Wayland portal question |
| **P1** | Recorder core behind cargo feature `recording` (joins `full`): the child and its identity rule, own capturer + cursor, CFR pacer, `RateMode`/GOP profile, fMP4 + remux + reconciler, audio (system + microphone → Opus), sidecar, guards, write rule. macOS: `NSMicrophoneUsageDescription` (the `Info.plist` heredoc, `release-agent.yml` ~:2417) and the `com.apple.security.device.audio-input` entitlement — neither exists, and `docs/remote-control.md:923-925` wrongly says the entitlement is covered. The designated requirement does not change, so existing Screen Recording grants must survive; verify on the MacBook, because FR-80 was a TCC break. | kept out of the release feature lists until P2; `ROOMLERD_RECORDING=0` | **P1a shipped**: `roomlerd record`, own capturer, CFR pacer + TickFifo, openh264 recording profile (HW backends: the live cascade with a forced GOP), fMP4 + remux + reconciler, folder rules, sidecar, the prefixed JSON-lines protocol; `tests/recorder.rs` named in CI; `docs/recording.md`. **P1b shipped**: `FfmpegEncoder::new_recording` — the live option sets (field-proven) with `cq` 19, a recording ceiling (NVENC ~0.3 bpp·s as a burst cap over CQ; the maxrate-anchored backends ~0.12 bpp·s as the bitrate), a real GOP and counter-based PTS; `hardware` refuses rather than fall back to software; verified locally by `ffprobe` on `h264_nvenc` (High, BT.601 tagged, keyframes at 0/60, zero decode errors). Open: true per-backend CQ modes (QSV ICQ, AMF/VAAPI CQP) after field measurement, **P1c** audio (system + mic → Opus) and the macOS plist/entitlement, **P1d** the cursor on non-WGC backends, **P1e** the identity-rule spawn (until it lands, a SYSTEM/root daemon refuses a local recording, P2a) |
| **P2** | Local: `record_*` keys, validators and the `DesiredConfig` prefix exclusion; LocalAPI verbs with the session peer check; the Recordings view, settings, folder picker and tray REC; the CLI; updater defer; delivery from the data dir | roomler-desktop greys Start out, with the reason, against a service without the recorder or one that cannot record here (`RecordingState.available`), and says "no screen recorder yet" to one that predates the verbs | **P2a built**: `record_dir` (live, one shared validator, structurally absent from `DesiredConfig`); the five LocalAPI verbs behind the console-user gate in the listener; `RecordingManager` (spawn, event folding, one at a time, `recorder_exited`); the SYSTEM/root refusal until P1e; reconcile at every recorder start, guarded by `PartialLock`; updater defer (`active_work`); `roomler record start\|stop\|status\|ls\|rm`. **P2b built**: roomler-desktop's Recordings view (start/stop and options, the folder with Change/Default/Open, the list with Play/Show/two-click Delete), the native folder picker, the tray's Start/Stop item and tooltip, `available`/`unavailable_reason` on the state, a daemon answer final for `record_*` keys, the companion's unit tests in a CI lane at last. Open: **P2c** delivery from the data dir (the Linux user unit's `ProtectHome=read-only` keeps the recorder out of `~/Videos`; the folder rules move to agent-core so the companion can apply them); a red tray icon while recording lands with P3's host indicator |
| **P3** | Remote: role bit, `may_record`, hub strip (break-glass included), `AgentCaps.record`, the `record` DataChannel, `PromptKind::Record`, indicator before the first frame, grace re-attach, list/get with resume, `RecordingActivity` + collection + baseline, viewer UI | device gate default **OFF**; the role bit granted to nobody by default; the `remote` module switch makes the hub strip RECORD | — |
| **P4** | One vendored-FFmpeg bump: `--enable-decoder=h264,hevc --enable-encoder=aac` on Windows (`vendor-ffmpeg-windows.yml` `$inject`), Linux (its configure + `probe.c`) and macOS (`vendor-ffmpeg-macos.yml`); invert the absence assertions (`ff_h264_decoder` becomes required, `ff_mov_muxer` stays forbidden); update `THIRD-PARTY-NOTICES.md`, `docs/encoders.md`, `docs/lgpl-relink.md`, `docs/fr/FR-24-*.md`; dispatch `lgpl-source-offer.yml` before tagging; re-pin the MSI baseline; AAC becomes the audio codec (arm64 keeps Opus) | a new asset suffix flipped in `release-agent.yml` (rollback = flip back) | — |
| **P5** | Export engine and the Edit view; the vmtest `record` cell | Edit hidden when `roomlerd media --version` fails | — |
| **P6** | Docs in the house style, the field matrix, close | — | — |

## Acceptance criteria

- [ ] **AC1 — local HQ recording.** On Windows, macOS and Linux x64, a 10-minute local recording at native resolution,
      30 fps, H.264 4:2:0, lands in the default folder as a progressive MP4 (`moov` before `mdat`) that plays in the OS's
      default player; its frames are native resolution while a concurrent session is relay-capped; the sidecar's
      dropped-frame count is ≤ 0.5 %.
- [ ] **AC2 — the folder.** The default resolves per OS as specified, and on a host with OneDrive-redirected or
      CFA-protected Videos it falls back with the reason shown; an override set in roomler-desktop is used by the next
      recording and "Reset" restores the default; a `record_dir` that is relative, UNC or contains a reparse point is
      refused at `ConfigSet` with a named reason.
- [ ] **AC3 — audio.** System audio and the microphone are off unless chosen; when both are chosen they stay within
      ±40 ms of the picture over a 30-minute recording (a clap test at start and end); a blocked microphone (Windows
      privacy switch, macOS TCC denied) refuses the start with `mic_unavailable{…}` rather than recording without it.
- [ ] **AC4 — crash safety.** `kill -9` of the recorder loses ≤ 2 s; the boot reconciler turns the partial into a
      playable file marked `interrupted`; a pushed update mid-recording ends the same way.
- [ ] **AC5 — remote gates.** Each gate refuses with its named reason on a real pair, every refusal first shown on the
      current deploy: no role bit · device gate OFF · a break-glass session · a pre-feature agent · a denied prompt ·
      an attended host with no indicator surface. With every gate passed, recording starts only after the host's banner
      and red dot are up, and the host's Stop ends it.
- [ ] **AC6 — remote download.** The controller downloads a ≥ 2 GiB recording made on a relay-carried session and
      survives a forced mid-transfer flap by resuming from the offset; the SHA-256 equals the host's file; a different
      controller of the same device can neither list nor fetch it.
- [ ] **AC7 — the microphone is local-only.** No wire message, CLI flag or API lets a remote controller turn on the
      host's microphone — a test per entry point, and a review that lists every entry point.
- [ ] **AC8 — editor.** An export with a cut, a ×4 speed-up and background music (MP3 and M4A, looped, with fades) has
      the expected duration ± 1 frame, the music at the chosen level, and plays in the OS default player; the
      `export.rs` oracle and its negative control both behave in CI.
- [ ] **AC9 — size.** The P4 release run's measured installer sizes are recorded in the field log; the MSI stays under
      the 18 MiB ceiling, or the ceiling moves in a PR that says why; the MSI baseline is re-pinned from that run.
- [ ] **AC10 — the server never carries a recording.** No code path uploads recording bytes; `recording_url` stays
      `None`; the server stores only `recording_activity` claims, and a test asserts the claim payload has no content
      field.
- [ ] **AC11 — tests run where they claim to.** `--test recorder`, `--test control_dc_record`, `--test export`, the
      `crates/tests` `recording_tests` module and the Playwright specs each run in a named CI lane, and each oracle's
      negative control has been shown RED once.
- [ ] **Docs** — `docs/recording.md` created in the house style (a mermaid component diagram, the remote start/stop
      sequence with every gate, the write-rule flowchart); `docs/remote-control.md` §11.3, `docs/permissions.md`,
      `docs/encoders.md`, `docs/installation.md`, `docs/data-model.md`, `docs/lgpl-relink.md` and `CONTEXT.md` updated;
      the page linked from `docs/README.md`.

## Test plan

⚠️ `agents/roomlerd/tests/*.rs` **never run in CI today**: every `cargo test -p roomlerd` step is `--lib`
(`.github/workflows/ci.yml:406,460,773`), and `tests/file_dc.rs` is in no workflow. Each new test binary below gets its own
named `--test` step. Every oracle has a **negative control** — the same assertions must fail on the wrong input.

- **Unit** (the default and `ffmpeg-encoder` roomlerd lanes; `crates/remote_control`, `crates/db`, `crates/localapi`):
  edit-list validation and PTS-based selection (property test; audio sample counts derived rationally — 48000/30 is an
  integer); `mp4-atom` avcC/hvcC from Annex-B, keyframe-aligned fragments, a remux that keeps every sample with `moov`
  first; the write-rule decision table and reparse/symlink refusal; drift corrector, soft-clip mix, gain/fade/loop;
  per-backend recording option sets (golden) and a lock that the live sets are unchanged; `PromptKind::Record` text, a
  prompt id that is not the session id, the cooldown; the `record` capability only while the gate is on; serde defaults
  and wire names; `DesiredConfig` refuses every `record_*` key; the role bit sits in no managed row below
  ADMINISTRATOR and `ALL` includes it; the new LocalAPI verbs reach `handle()` and the peer check refuses another
  session.
- **Agent integration** (`agents/roomlerd/tests/`): `recorder.rs` — five seconds at 30 fps of the counter-strip source
  gives a progressive MP4 of 150 ± 1 frames and 5.0 s ± 1 frame whose decoded counters are in order; `kill -9` leaves a
  staged fMP4 that parses to its last fragment and that the reconciler finalizes; stdin EOF finalizes cleanly; the low
  water mark stops it `disk_low`. `control_dc_record.rs`, on the `file_dc.rs` loopback-PeerConnection pattern with the
  production handlers — every refusal reason, list isolation between controllers, `get` SHA-256 and resume, path
  confinement, the microphone refused remotely, the gate going OFF mid-recording. `export.rs` — a 10 s synthetic
  recording, a 440 Hz WAV generated in the test and the edit list keep [0,2) · cut [2,4) · ×4 [4,8) · keep [8,10) decode
  to the counters `0..59 ++ 120,124,…,236 ++ 240..299` and 5.0 s ± 1 frame, with audio in band; the unedited source must
  fail the same assertions.
- **Server integration** (`crates/tests`, new `recording_tests.rs`): RECORD is kept iff (owner, ADMINISTRATOR or the
  role bit) and the capability and no `override_reason`, every cell with a positive control; a `RecordingActivity`
  for a session without RECORD is rejected and lands nowhere; and a **full path** — an in-process agent built with
  `synthetic-frame-source,openh264-encoder,recording` and a webrtc-rs controller record three seconds, download them and
  parse the counters.
- **E2E** (Playwright in the in-pod `pwrunner` sidecar, `scripts/e2e-nightly.sh`; `Dockerfile.agent-e2e` gains
  `recording`): `remote-recording-smoke.spec.ts` (Record → REC chip → Stop → the list → Download → an MP4 box walk in
  the spec → the device activity shows start and stop); `remote-recording-denied.spec.ts` (no role bit, gate off,
  break-glass: no button; the admin control case sees it); roomler-desktop's frontend served in Playwright Chromium with a
  mocked Tauri bridge (start/stop/list, folder default/override/reset, editor → golden edit list, export progress and
  cancel); the vmtest `desktop` check goes from five views to six, plus a `record` cell that records, probes, exports and
  probes the output on real OSes.

## Open decisions

1. **Wayland portal** — a silent second session through the restore token, or a tee of the worker's stream? P0 measures.
2. **HEVC editing in v1** (+≈ 0.4 MB compressed) — decided at P0's exit from measured numbers, not from this estimate.
3. **Owner shortcut for `may_record`** — proposed yes (the organisation's owner may record its own devices, still
   subject to gates 3–6).
4. **The re-attach grace** — 60 s proposed; tune against the reconnect ladder's measured timings.
5. **Quality numbers per backend** (the `q` of each row above) — tuned in P1 on a text-heavy scroll and a video clip.

## Out of scope

Separate audio stems · a webcam overlay, annotations, click highlights, automatic speed-up of idle stretches ·
choosing a monitor (capture is primary-only today) · recording conference calls · an editor in the browser · any
server-side storage or upload of recordings (the plaintext invariant forbids it) · macOS system audio (needs
ScreenCaptureKit and macOS 13) · importing arbitrary videos into the editor.

## Field-verification log

| Date | Where | Phase | Read |
|---|---|---|---|
| 2026-09-25 | release `agent-v0.4.101` assets | P0 baseline | MSI 16,109,568 B (= 15.36 MiB in the guard's units) · Linux x64 .deb 13,830,684 B · arm64 .deb 11,410,760 B · macOS .pkg 21,765,134 B · roomler-desktop .exe 15,896,864 B · roomler-desktop .deb 4,422,496 B |
| 2026-09-25 | dev box, 0.4.101 installed | P0 baseline | `roomlerd.exe` 43,720,480 B (.text 32,041,105); the MSI's cab is 16,041,977 B holding 45,280,568 B, so MSZip ≈ 0.35. Vendored Windows FFmpeg: `avcodec.lib` 21,812,318 B, `avformat.lib` 3,217,828 B (32 members, no muxer or demuxer), `swresample.lib` 1,031,918 B (built, unused) |
| 2026-09-25 | `release-agent.yml` | P0 baseline | the MSI guard's pinned baseline (13.13) is stale — 0.4.101 is 2.23 over it — so its +2 warn band already fires before this FR adds a byte |
| 2026-09-25 | vendor workflows on `fr84-p0-measure` ([Windows + Linux](https://github.com/gjovanov/roomler-ai/actions/runs/36148229193), [macOS](https://github.com/gjovanov/roomler-ai/actions/runs/36148233530)) | P0 | all three trees built with `--enable-decoder=h264,hevc --enable-encoder=aac`; the new probes found all three components present and prores still trimmed. Linux `libavcodec.so.63.1.101` 1,659,312 → 3,797,744 B (**+2.04 MiB**, xz-9 +0.47 MiB); macOS `libavcodec.63.1.101.dylib` 790,600 → 2,731,480 B (**+1.85 MiB**, xz-9 +0.42 MiB, deflate +0.69 MiB); Windows `avcodec.lib` 21.8 → 38.0 MB (archive size, not a link delta) |
| 2026-09-25 | dev box, MSVC 14.50 | P0 | the Windows link delta, measured: one probe calling `avcodec_find_encoder_by_name` (which pulls the whole codec registry) linked statically against the production tree = 1,439,232 B (`h264dec=NULL`) and against the P0 tree = 4,369,408 B (a live `h264dec`): **+2.79 MiB linked, +0.82 MiB deflate-6** (the MSZip proxy) |
| 2026-09-25 | dev box, Windows | P1a | `cargo test -p roomlerd --features recording,openh264-encoder,synthetic-frame-source`: 47 unit tests and `--test recorder` 8/8, green three runs running. Negative controls shown RED before trusting them: chunk order reversed in the remux → `remux_keeps_every_sample_in_order_with_moov_first` fails; the recorder frozen on its first frame → the end-to-end oracle reads "1 of 91" distinct frames and fails |
| 2026-09-25 | dev box (Windows, NVIDIA), `roomlerd record` on the synthetic source | P1b | `--encoder hardware` → `h264_nvenc` opened in the recording profile; `ffprobe`: High, 320×240, `yuv420p`, `color_range=tv`, `smpte170m` ×3, 30/1 fps, 116 frames / 3.867 s, keyframes at frames 0 and 60, `ftyp → moov → mdat`, `ffmpeg -f null` decode with zero errors. `--encoder software` → openh264: Constrained Baseline, same colour tags, keyframes 0/60, zero decode errors. Recordings deleted after the check |
| 2026-09-25 | dev box (Windows) + WSL Ubuntu 24.04 | P2a | Windows: `--test recorder` 12/12, `--lib recording::` 51, `roomler-localapi` 27, `roomler-node-core` 184, `remote_control` 194, the CLI's record tests; clippy `-D warnings` with and without `recording`. Linux (WSL): fmt, `roomler-localapi` clippy + tests (the unix peer-uid half of the gate, which a Windows build never compiles), agent-core, CLI, and the recorder's clippy, lib and `--test recorder`. Negative controls shown RED before trusting them: the partial lock disabled → the reconciler finalized a LIVE recording's partial; the start-deadline kill disabled → the child reported `started` after its caller had been told it did not start |
