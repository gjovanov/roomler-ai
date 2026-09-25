# Screen recording

> **FR-85** ([#1634](https://github.com/gjovanov/roomler-ai/issues/1634),
> [spec](fr/FR-85-hq-screen-recording.md)). **Status: P1 (the recorder core,
> and its audio on Windows and Linux), P2a (the local verbs), P2b
> (roomler-desktop's Recordings view and tray), P3a (the server's gates for
> remote recording) and P3b (the device's half of it, §10).** It sits behind
> the `recording` cargo feature and is in no release build yet. It is driven by
> `roomlerd record`, by the daemon for the LocalAPI recording verbs and
> `roomler record` (§6), by roomler-desktop (§7), and by a remote controller
> over the session's `record` channel (§10). Still to come: the microphone on
> macOS, delivery out of the recorder's data folder (P2c), downloading a remote
> recording and re-attaching after a drop (P3b-2), the viewer's Record button
> (P3c), and the editor (cut, speed up, background music) in P5.

A recording is **encoded at the source, in a pipeline of its own, into a
local file.** It is not a copy of what a viewer receives. The live
remote-control stream is rate-controlled to fit the network and its capture is
capped by the viewer's rung, so recording it would give transport quality.

## 1. The pipeline

```mermaid
flowchart LR
    subgraph child["roomlerd record (its own process)"]
        CAP["capturer<br/>capture::open_default<br/>(native resolution)"] -->|latest frame wins| W[("watch")]
        W --> P{{"pacer tick<br/>1/fps on its own clock"}}
        P -->|"same frame again<br/>on a still screen"| ENC["encoder<br/>recording profile"]
        ENC -->|"Annex-B access units<br/>(TickFifo → PTS)"| FW["FragmentedWriter<br/>moof+mdat per GOP"]
    end
    FW -->|"stop / guard / parent gone"| FIN["finalize<br/>remux → moov-first MP4"]
    FIN --> OUT[("Roomler Recording ….mp4<br/>+ .roomler.json")]
    CRASH["recorder killed"] -.-> PART[(".roomler-partial/….partial")]
    PART -.->|"reconcile_partials"| FIN
```

| Stage | Code | Why it is shaped this way |
|---|---|---|
| Capture | `recording/recorder.rs` capture task | Its **own** capturer, never a tap on the live pump (`peer.rs`), which is capped by the viewer's rung and is the most-tuned path in the product. A second capturer costs a second WGC session, or one of DXGI's ~4 duplication seats. Only the latest frame is kept, so a slow encoder never backs capture up. |
| Pacer | `recording/pacer.rs` `Cadence`, `TickFifo` | A backend's `Frame.monotonic_us` has a different origin per backend (wall-clock on the portal), and capture is change-driven. The recorder stamps frames on its own `Instant`, one tick per `1/fps`, and repeats the last frame on a still screen. An encoder that falls behind leaves a gap in tick numbers: the previous frame lasts longer, and the file never runs fast. |
| Encoder | `encode/openh264_backend.rs` `new_recording` / the H.264 cascade | See §3. |
| Writer | `recording/mp4.rs` `FragmentedWriter` | Fragmented while recording, so a crash loses at most one fragment (~2 s). See §2. |
| Finalize | `recording/mp4.rs` `finalize` | A remux to a progressive, `moov`-first file that QuickTime, Movies & TV and editors open. Payloads are copied, never re-encoded. |

⚠️ **A process of its own.** `roomlerd record` is a child, like the capability
probe, so a driver fault in a recording's encoder costs the recorder, never
the daemon or a live session. It stops when its stdin closes, so a recorder
never outlives whatever launched it.

## 2. The file

```mermaid
flowchart TB
    subgraph rec["while recording — fragmented"]
        direction LR
        F1["ftyp"] --> M1["moov<br/>(empty tables + mvex)"] --> A1["moof"] --> D1["mdat<br/>GOP 1"] --> A2["moof"] --> D2["mdat<br/>GOP 2"] --> DOTS["…"]
    end
    subgraph done["on stop — progressive"]
        direction LR
        F2["ftyp"] --> M2["moov<br/>stts · stss · stsz · stsc · stco/co64"] --> D3["mdat<br/>every GOP, in order"]
    end
    rec -->|"finalize: payloads copied, not re-encoded"| done
```

- **A fragment starts on every keyframe** (GOP 2 s). A 10 s backstop cuts one
  anyway for an encoder that stops producing keyframes, so a crash never loses
  more than that.
- `flush` after every fragment, which is all a *process* crash needs.
  `sync_data` every fifth fragment, for power loss.
- Samples are `avc1` with the parameter sets in `avcC` and removed from the
  samples; AUDs are dropped. ⚠️ If the parameter sets change mid-recording the
  writer **refuses** rather than write an `avc1` file that lies about itself.
  The recording then ends `encoder_failed`.
- `colr` (nclx) says **BT.601, limited range**. That is what every backend's
  BGRA→YUV conversion produces (openh264's own, dcv on the FFmpeg path,
  `encode/color.rs` for MF), and openh264's recording profile signals the same
  thing in its SPS VUI. Without it an HD player assumes BT.709 and shifts
  every colour.
- **Crash recovery.** `read_fragmented` walks top-level boxes and stops at
  the first incomplete fragment. `reconcile_partials` remuxes whatever a dead
  recorder left, marks it `interrupted`, and removes a partial that holds
  nothing recoverable.
- If the remux itself fails (for example, no room for a second copy), the
  fragmented file is kept under the final name and the `stopped` event says
  `fragmented: true`.

## 3. Encoders

| Preference | Encoder | GOP | Rate |
|---|---|---|---|
| `software` | openh264, **recording profile** (`Openh264Encoder::new_recording`) | its own, `fps × 2` | quality mode, QP 10–30, ~0.2 bpp target (4–40 Mbps), no frame skipping, screen-content tuning |
| `hardware` | the FFmpeg H.264 cascade in its **recording profile** (`FfmpegEncoder::new_recording`); refuses rather than fall back to software | its own, `fps × 2` | see below |
| `auto` | `hardware`, then `software` | | |

**The hardware recording profile** reuses the live option sets: every one of
them is field-proven on the fleet's GPUs, and a new private-option dict would
be a new way for a driver to refuse an open. Three things change:

- **Quality target:** `cq` 19 instead of the live 22 (`ROOMLERD_RECORD_CQ` overrides).
- **Ceiling** (`recording_maxrate_bps`):
  - on NVENC, which runs constant quality with `bit_rate=0`, it is only a burst ceiling, so it is generous (~0.3 bpp·s, 10–80 Mbps);
  - QSV, AMF, VAAPI, D3D12, Vulkan and VideoToolbox anchor their rate control *on* `maxrate` (QSV opens as CBR when `b:v == maxrate`), so there the ceiling is the bitrate (~0.12 bpp·s, 6–40 Mbps: 1080p30 ≈ 7.5 Mbps, 4K30 ≈ 30 Mbps).
- **GOP and timing:** a real GOP replaces the on-demand-only `KEYFRAME_INTERVAL`, and PTS come from the encoder's own frame counter, because a recording feeds the same frame again on a still screen.

Every live open passes `gop_override: None` and keeps the capture clock, so
the live path is unchanged. True per-backend constant-quality modes (QSV ICQ,
AMF/VAAPI CQP) are a follow-up that needs field measurement on each vendor's
hardware first.

Verified locally with the synthetic source, by `ffprobe`:

| Encoder | Profile | Colour | GOP |
|---|---|---|---|
| `h264_nvenc` | High | `smpte170m`/`tv` | keyframes at 0 and 60 |
| openh264 | Constrained Baseline | `smpte170m`/`tv` | keyframes at 0 and 60 |

Both are 30 fps constant, decode with zero errors, and are laid out `ftyp → moov → mdat`.

⚠️ **The encoder denylist applies here too.** The FFmpeg constructors read
`ENCODER_CELLS_DENY` through `node_env`. A child the daemon spawns inherits it
as env; `roomlerd record` run by hand reads it from the config file
(`register_encoder_fallbacks`). A gate the probe and the live session honour
but the recorder skipped would only be a courtesy.

## 4. Where it goes

| | Windows | macOS | Linux |
|---|---|---|---|
| Default | `Videos\Roomler`, **only if** local, not under OneDrive, and it survives a real write probe | `~/Movies/Roomler` | XDG videos dir + `Roomler` |
| Fallback | `%USERPROFILE%\Roomler Recordings` | same rule | same rule |
| Last resort | the recorder's data dir (`%LOCALAPPDATA%\…\recordings`, never roaming) | `~/Library/Application Support/…/recordings` | `~/.local/share/roomler/recordings` |

- ⚠️ **The probe is a real write.** Defender's Controlled Folder Access lets
  `metadata()` succeed and then blocks the write. OneDrive's Known Folder Move
  would upload gigabytes of screen recordings by default.
- **Override** (`--out`, or the config key `record_dir`): absolute, local, no
  `~`, no UNC or `\\?\` device path, no `..`, no symlink or junction component.
  ONE validator, `roomler_node_core::recording_dir::validate_record_dir`
  (`crates/agent-core/src/recording_dir.rs`), shared by the config surface and
  the child. On Windows `is_symlink` is true for junctions and false for cloud
  placeholders. That is the distinction wanted: a placeholder doesn't redirect
  a path, a junction does.
- **`record_dir` is live.** The daemon reads it when a recording starts, so the
  surface marks it `restart_required = false`. It is **device-only**: a
  `DesiredConfig` cannot carry it, because the struct has no `record_*` field
  at all. `no_record_key_is_server_pushable_via_desired_config` locks that.
  Where screen recordings are kept is the owner's choice. A server that could
  move the folder could move them into a synced or shared one.
- **Names never collide:** `Roomler Recording 2026-09-25 14-30-12.mp4`, then
  ` (2)`, ` (3)`, … A partial with the name counts as taken.
- **Staging:** `<folder>/.roomler-partial/<name>.partial`, renamed into place on
  finish. The file is created with `create_new`: a recording never replaces
  anything. Beside it sits `<name>.partial.lock`, an OS file lock the recorder
  holds until the file is final (see §5, crash recovery).

## 5. `roomlerd record`

```
roomlerd record [--out <dir>] [--fps 30] [--encoder auto|hardware|software] [--max-minutes 240]
                [--system-audio] [--microphone]
```

**stdout:** one JSON object per line, each prefixed `ROOMLER_REC_JSON:`. That
is the probe children's convention (`ROOMLER_CAPS_JSON:`): the daemon's tracing
writes to the same stdout, and a parent that parsed every line would choke on
the first log line. Use `recording::child::parse_event_line`.

| `ev` | When | Fields |
|---|---|---|
| `folder` | the default or configured folder was not used | `path`, `reason` |
| `started` | the first frame is encoding | `partial`, `path`, `width`, `height`, `fps`, `encoder`, `system_audio`, `microphone` |
| `progress` | every second | `duration_ms`, `bytes`, `frames`, `late_ticks` |
| `stopped` | the file is final | `reason`, `path`, `bytes`, `duration_ms`, `frames`, `fragmented` |
| `refused` | it never started | `code`: `no_frame` · `disk_low` · `encoder_unavailable` · `folder_unwritable` · `audio_unavailable` · `system_audio_unavailable` · `mic_unavailable` |
| `recovered` | a dead recorder's partial in this folder was finalized | `path` |

**stdin:** `{"cmd":"stop"}`, optionally with a `"reason"`. **End of stdin
stops the recording** (`parent_gone`), and so does Ctrl+C. Every stop
finalizes.

**Stop reasons** (a closed set; the UI maps each to a sentence): `requested` ·
`host_stopped` · `session_ended` · `session_changed` · `display_changed` ·
`disk_low` · `max_duration` · `encoder_failed` · `capture_failed` ·
`gate_revoked` · `parent_gone` · `interrupted`.

Guards: the recorder refuses to start with under 2 GiB free, stops cleanly
under 1 GiB, and stops at the maximum length. A display that changes size ends
the file cleanly (`display_changed`) instead of scaling mid-recording.

**Crash recovery.** A recorder that died (a crash, `kill -9`, the Windows
installer's Restart Manager during an update) leaves its partial in the staging
dir. Every `roomlerd record` runs `recorder::reconcile_partials` over its own
staging dir as it starts. That runs beside the new recording, never before it,
because a multi-GB remux must not hold up `started`. Each complete fragment is
remuxed into place, and the file is marked `interrupted` in its sidecar.

⚠️ **A partial is finalized only if its `.partial.lock` can be taken**
(`recorder::PartialLock`). The recorder takes that OS file lock before it
creates the partial, holds it until the file is final, and the kernel releases
it however the process dies. Without it the reconciler could not tell a dead
recorder's partial from a live one's. A second recorder in the same folder (a
manual `roomlerd record` beside the daemon's) would remux a file under its
writer, or, before the writer's first fragment, **delete it as
unrecoverable**. `a_live_partial_is_left_alone_by_the_reconciler` is red with
the lock disabled.

## 6. Recording through the daemon — the local verbs (P2a)

The person at the device records through the running daemon: from
roomler-desktop (P2b) or from `roomler record`. The daemon never records in its
own address space. It launches `roomlerd record` and follows its events
(`recording/manager.rs`, `RecordingManager`).

```mermaid
sequenceDiagram
    autonumber
    participant C as roomler record / roomler-desktop
    participant L as LocalAPI listener
    participant M as RecordingManager (daemon)
    participant R as roomlerd record (child)
    C->>L: RecordStart {fps, encoder, max_minutes}
    L->>L: peer is the console user?<br/>(else: refused, never reaches M)
    L->>M: record_start(opts)
    M->>M: SYSTEM/root daemon? refuse (P1e)<br/>one already running? refuse
    M->>R: spawn: record --config … [--out record_dir]<br/>env: config fallbacks (the encoder denylist)
    R-->>M: ROOMLER_REC_JSON:{"ev":"started",…}
    M-->>C: Recording {active, path, encoder, w×h@fps}
    loop every second
        R-->>M: progress {duration_ms, bytes, frames}
    end
    C->>L: RecordStop
    L->>M: record_stop()
    M->>R: stdin {"cmd":"stop"}
    R-->>M: stopped {reason, path, bytes, …} (file final)
    M-->>C: Recording {active: false, last}
```

| Verb | Caller | Answers |
|---|---|---|
| `RecordStart {opts}` | console user | `Recording` once the child reports `started` (≤ 20 s), or an error naming why it did not start |
| `RecordStop` | console user | `Recording` once the file is final (the remux copies every byte once; ≤ 5 min) |
| `RecordStatus` | any LocalAPI client | `Recording`: active, file, length, size, frames, encoder, and how the last one ended |
| `RecordingsList` | any LocalAPI client | `Recordings`: the folder, why it is that one, and each file with its sidecar facts, newest first |
| `RecordingDelete {name}` | console user | `RecordingDeleted`; a bare `*.mp4` name only, a regular file only (never a link), refused while it is being written |

- ⚠️ **The console-user gate is the listener's, not the handler's**
  (`serve_connection_as`, `crates/localapi/src/lib.rs`). The pipe ACL admits
  every Interactive User, RDP sessions included, which is right for reading
  status and wrong for recording: a guest in an RDP session must not be able
  to record the console user's desktop. Windows: the client's session
  (`GetNamedPipeClientProcessId` → `ProcessIdToSessionId`) must be the active
  console session. Unix: the peer's uid must be the daemon's, or root. An
  unidentified peer fails the check. The same gate covers `ConfigSet` of any
  `record_*` key.
- ⚠️ **A SYSTEM/root daemon refuses a local recording, and says why**
  (`recording/identity.rs`). The child inherits the daemon's identity, so from
  the Windows SystemContext worker or a Linux/macOS root daemon the recording
  would land in the service account's own profile
  (`…\systemprofile\Videos`, `/root/Videos`), where the person who pressed
  Record cannot see it. P1e launches the recorder as the console user. Until
  then, `roomlerd record` run in your own session still works.
- **One recording at a time.** A second start answers an error. A child that
  exits without saying how it ended (a crash, bad arguments, a binary built
  without the recorder) ends as `recorder_exited` with a sentence, never as a
  silent "did not start".
- ⚠️ **A recorder that misses the start deadline is killed, never left
  running** (`start_timeout`). Its caller was told it did not start. Stuck in an
  encoder open, it could otherwise begin recording a minute later, unseen. A
  stop command would not reach it, because the recorder reads its stop only once
  it is recording. `a_recorder_that_misses_the_start_deadline_is_stopped_not_left_recording`
  is red without the kill: the child reported `started` after its caller had
  been told no.
- **Updates wait for a recording** (`updater.rs`, `active_work`). An active
  recording counts as in-flight work, like a file transfer, with the same defer
  budget. An operator-forced update proceeds anyway and says so. The mark
  (`manager::is_recording`) is a count each recorder's reader task adds to once
  and removes once, so no code path can clear a recording it does not own.
- **`roomler record start|stop|status|ls|rm`**
  (`agents/roomler-cli/src/cli.rs`) wraps these verbs one to one. `--json`
  prints the wire shape.

## 7. roomler-desktop — the Recordings view and the tray (P2b)

The companion's sixth view (`#/recordings`, `src/front/recordings.js`) and a
tray item. Both only ASK the daemon; every decision stays there (§6).

```mermaid
flowchart LR
    subgraph companion["roomler-desktop (the person's session)"]
      V["Recordings view<br/>recordings.js"]
      T["tray item<br/>Start / Stop recording"]
      P["native folder picker<br/>cmd_pick_record_dir"]
    end
    subgraph daemon["roomlerd (LocalAPI)"]
      G{"console user?"}
      M["RecordingManager"]
      C["config surface<br/>record_dir (live)"]
    end
    V -- "cmd_recordings_view:<br/>status + list + record_dir,<br/>ONE connection" --> G
    V -- "cmd_record_start / stop / delete" --> G
    T -- "RecordStatus every 3 s,<br/>Start / Stop" --> G
    P -- "a folder" --> V
    V -- "cmd_config_set record_dir" --> G
    G --> M
    G --> C
```

| Part | What it does |
|---|---|
| **Record this screen** | Start / Stop, the frame rate (30 or 60) and the encoder (automatic, GPU only, software only), a blinking REC chip with the running time, size, encoder and resolution. The options lock while a recording runs. How the last one ended is one sentence (`describeLast`); every closed stop reason and refusal code has words. |
| **Where recordings are saved** | The folder in use, whether it is the default or one the person chose, **Change folder…** (the native picker), **Use the default folder**, and **Open folder**. A fallback is named ("Not the usual folder: … is under OneDrive"). |
| **Saved recordings** | Newest first, from the sidecars: when, how long, size, by whom (this device, or a remote controller by name), how it ended on hover. **Play** (the OS's default player), **Show** (selected in the file manager), **Delete** (two clicks, no modal: the first arms it for 4 s). |
| **Tray** | **Start recording** / **Stop recording (m:ss)**, and the tooltip `Roomler — recording m:ss`, following the recorder whoever started it. A refusal opens the Recordings view with the sentence. |

- **Start is greyed out, with the reason, wherever the service cannot record.**
  `RecordingState` carries `available` and `unavailable_reason` (additive,
  serde default `false`): false on a service built without the recorder, and
  on a SYSTEM/root service until P1e. The tray disables its item the same way,
  so neither offers a button that can only fail.
- **One LocalAPI connection per refresh** (`cmd_recordings_view` reads status,
  list and `record_dir` in turn), every second while a recording runs, every
  5 s otherwise, and only while the view is visible. A failed refresh keeps
  the last good data and says why (the FR-84 D1 rule). The rows are keyed and
  patched in place, so a button never moves under the cursor.
- ⚠️ **A daemon's answer on a `record_*` key is final.** `cmd_config_set`
  falls back to writing the config file itself when the daemon fails. For the
  recording keys it no longer does once the daemon has *answered*: the
  listener refuses anyone but the console user, and a direct write behind that
  refusal would report a success the daemon refused. (A daemon that is not
  running still falls back: that is the person editing their own file.)
- **Play / Show open a NAME, never a path from the page.** `cmd_recording_open`
  joins the daemon's own folder with a bare `*.mp4` name, and opens it only if
  it is a regular file. On Windows `/select,` needs the path quoted *inside*
  the argument, which standard argument quoting breaks, so it is passed raw.

## 8. The sidecar

`<recording>.roomler.json`, beside the file: who started it (`local`, or
`remote` with the controller's **user id**, because remote downloads are
owned by user and not by session id), start and end times, size and fps,
codec and the backend that encoded it, colour, audio sources, frames,
late ticks, events, the stop reason, and bytes. ⚠️ **Never content**: no
window titles, and nothing typed.

## 9. Audio (P1c)

Computer audio and the microphone are both **OFF unless asked for**
(`--system-audio`, `--microphone`; the checkboxes in §7; `RecordStartOpts`).
The microphone is local-only: no remote path will ever set it (P3).

```mermaid
flowchart LR
    S["computer audio<br/>WASAPI loopback / Pulse monitor<br/>(never a mic)"] --> RS["resampler<br/>linear, → 48 kHz stereo,<br/>rate trimmed by depth"]
    M["microphone<br/>default input"] --> RM["resampler"]
    RS --> BS["buffer ≈ 60 ms"]
    RM --> BM["buffer ≈ 60 ms"]
    C(("the recorder's clock<br/>(the video pacer's Instant)")) --> X
    BS --> X["mixer: one 20 ms frame<br/>per 20 ms of clock,<br/>silence for what is missing,<br/>soft-clipped sum"]
    BM --> X
    X --> O["Opus 128 kb/s<br/>(its own encoder)"] --> Q["queue until the first<br/>video frame is written"] --> W["MP4 audio track"]
```

| Decision | Why |
|---|---|
| **The mixer PULLS on the recorder's clock** | WASAPI loopback delivers nothing while nothing plays, and every device clock drifts. A track timed by what the devices delivered would collapse during silence and walk away from the video. Pulling one frame per 20 ms of the video pacer's own `Instant` makes audio time video time by construction. |
| **It runs 60 ms behind** (`MIX_LAG`) | Devices hand over ~10 ms bursts. A mixer level with the clock would pad silence into every frame. |
| **Drift is corrected by RATE** | Each source's linear resampler is bent by at most ±0.5 % toward keeping its buffer at the lag. Padding alone would click: a device clock 0.1 % slow empties the buffer, and from then on every frame pads a sample. The hard trim (a buffer past 250 ms, cut back to 60 ms, newest kept) is only for backlogs such as the capture pre-roll at start. |
| **Linear, not nearest-neighbour** | A 44.1 kHz laptop microphone is common, and the live path's nearest-neighbour aliases audibly on speech. The live path keeps its own resampler, untouched. |
| **Audio waits for the first video frame** | The writer's audio decode time starts at the first packet pushed after its header. Encoded audio queues from the clock's start; when the first video frame is written, frames mostly before its time are dropped, so the tracks start within ±10 ms of each other. |
| **Soft clip, not wrap** | Two loud sources sum past full scale. Up to ¾ of full scale the sum is untouched; above it, a `tanh` knee compresses toward full scale, settling at it only far past it (never beyond, never wrapping). |
| **A source asked for and not opened refuses the start, by name** | `system_audio_unavailable` (no loopback or monitor: set `ROOMLERD_AUDIO_SOURCE`), `mic_unavailable` (none, or on Windows the "Let desktop apps access your microphone" switch), `audio_unavailable` (a build without the `audio` feature; macOS today). A recording that silently lacked the audio the person asked for would be worse. |
| **A source lost mid-recording is not fatal** | The recording goes on with that source's silence. The sidecar records `audio_source_lost`, or `audio_failed` if the encoder failed and the rest is video-only. |

⚠️ **Computer audio never falls back to a microphone.** The live
remote-control path, on a Linux host without a PulseAudio monitor, falls back
to the default INPUT device. The recorder opens
`cpal_backend::Source::SystemOnly`, which refuses instead
(`system_audio_unavailable`).

⚠️ **Not yet on macOS.** Computer audio there needs ScreenCaptureKit (macOS 13+;
the bundle targets 12). The microphone needs cpal on macOS, plus
`NSMicrophoneUsageDescription` and the `audio-input` entitlement in the
bundle. Both are refused by name until then.

## 10. Remote recording (P3)

A controller in the browser records the screen it controls. P3a built the
server's gates, P3b the device side, and P3c builds the viewer. The file stays **on the device**, and
the controller downloads it on demand over the session's own P2P channel. The
server never holds a byte of it (`RemoteSession.recording_url` stays `None`). It
decides who may ask, and it keeps the device's account of what happened.

### The server's gates (P3a)

The session bit is `Permissions::RECORD`. Before P3a nothing read it, and the
hub passed through every bit a tab asked for except INPUT, so any tab could ask
for RECORD. Now the bit survives only when both gates below say yes, and every
refusal is named.

```mermaid
sequenceDiagram
    participant V as viewer (browser)
    participant C as controller's pod<br/>resolve_session_authz
    participant H as hub (the agent's pod)<br/>create_session
    participant A as agent
    participant DB as MongoDB

    A->>H: rc:agent.hello, or a heartbeat re-announcing caps:<br/>caps.record = ["remote"] only while its owner's gate is on (P3b)
    V->>C: rc:session.request, permissions "VIEW | … | RECORD"
    C->>C: may_record = owner ∨ ADMINISTRATOR ∨ RECORD_REMOTE_SCREEN,<br/>and false under break-glass
    C->>H: dispatch (or the cross-pod relay), carrying may_record
    H->>H: record_grant(permissions, may_record, supports_record)
    H-->>V: rc:session.created { permissions, record_refused? }
    H->>DB: remote_audit: SessionRequested { permissions, record_stripped? }
    H->>A: rc:request { permissions } — RECORD only if it survived
    A-->>H: rc:recording.activity { session_id, kind, … } (P3b)
    H->>DB: recording_activity — only for a session of THIS device whose grant held RECORD
```

| Gate | Where | Refusal |
|---|---|---|
| **1. The controller may record**: the device's owner, an `ADMINISTRATOR`, or a holder of the role bit `RECORD_REMOTE_SCREEN` (bit 31). The bit is in no managed role below `ADMINISTRATOR` ([permissions.md](permissions.md) §2) | `resolve_session_authz` → `SessionAuthz.may_record` (`crates/modules/remote/src/controller.rs`); `relay_rc_frame` carries it to the agent's pod | `controller_not_allowed` |
| ⚠️ **Never under break-glass.** An `ADMINISTRATOR` with an `override_reason` skips the host's consent, so a recording made that way would be covert ([remote-control.md](remote-control.md) §11.4) | the same function: its break-glass branch sets `may_record: false` | `controller_not_allowed` |
| **2. The device serves it**: its caps advertise `AgentCaps.record` ∋ `remote`. An agent does that only while its owner's `record_remote_enabled` is on (P3b). The hub keeps it per connection (`ConnectedAgent.supports_record`), set from the hello and refreshed from any heartbeat that re-announces caps (FR-43 P2c), so an owner's ON or OFF reaches the hub within one heartbeat, without a reconnect. It never reads it from the stored row, which outlives both an owner's OFF and a rollback | `record_grant` (`crates/modules/fleet/src/hub.rs`), applied after the INPUT rule | `device_not_opted_in` |

Everything downstream reads the **effective** grant: `SessionCreated.permissions`
(the viewer), `Request.permissions` (the agent) and the `remote_sessions` row.
The reason travels in `SessionCreated.record_refused` and in the audit's
`SessionRequested.record_stripped`, so a strip is never silent. When both gates
fail, `controller_not_allowed` is the one named. Both fields are additive and
absent when nothing was refused. ⚠️ A duplicate request on the same connection
coalesces onto the live session (#1045) and is answered with that session's
grant, so the session keeps its reason and the duplicate's answer repeats it.
Without that, a viewer's retry would read "RECORD was never asked for".

⚠️ **`remote` is a prefix of `remote-audio`** (the device also allows computer
audio in a remote recording). Matching is equality (`AgentCaps::has_record`),
locked by `remote_recording_does_not_imply_remote_audio`. It is the same lesson
as `ssh` and `ssh-consent`. A word this server does not know is ignored, never
an error. The microphone is not a remote option at all.

An agent older than P3b never advertises `record`, so every request for RECORD
to it is stripped with `device_not_opted_in`.

### The device's half (P3b)

The owner switches it on in roomler-desktop's Recordings view ("Allow remote
recording", and separately "Include computer audio"), or with
`roomler config set record_remote_enabled true`. Both keys are device-only: the
console gate accepts them only from the person at the device, and
`DesiredConfig` cannot carry them. Both are live. The agent re-announces its caps
on the next heartbeat, so the hub follows within one beat, and an OFF also stops
a remote recording in progress (`gate_revoked`). The agent advertises `remote`
only while the switch is on AND a recorder can run in its process, and
`remote-audio` only on top of that with the audio switch on and an audio build.

A controller holding RECORD opens a `record` DataChannel on the session
(`recording/remote.rs`). On a session whose grant lacks RECORD the channel
answers every request with `not_granted`, the same attach-time gate as `files`
and `input` (`peer.rs`).

```mermaid
sequenceDiagram
    participant V as viewer
    participant D as device (record DC)
    participant H as the host's screen
    participant R as roomlerd record

    V->>D: rc:record.start {id, audio?}
    D->>D: the owner's switch · a recorder here · audio allowed · not busy
    alt the session was consented ON THE HOST
        D-->>V: rc:record.state pending_consent
        D->>H: "Alice wants to record this screen" (a FRESH prompt id)
        H-->>D: approve / deny / no answer
    end
    D->>H: banner: "Recording your screen for Alice" + Stop recording
    Note over D,H: nothing can show it ⇒ refused {no_indicator_surface}
    D->>R: start_remote (never the microphone)
    D-->>V: rc:record.state recording {name}, then progress each second
    V->>D: rc:record.stop
    R-->>D: the file is final
    D-->>V: rc:record.state stopped {reason, bytes, duration_ms, name}
```

| Refusal | Why |
|---|---|
| `not_granted` | the session's grant lacks RECORD |
| `disabled_on_device` | the owner's switch is off, read at the moment of asking and again after the prompt |
| `unavailable` | no recorder can run in this process: a SYSTEM/root service until P1e, or a session delegated to the macOS GUI worker |
| `audio_not_allowed` | computer audio was asked for and the owner has not allowed it, or the build has no audio |
| `busy` | a recording is already running, local or remote |
| `already_starting` | this session is already starting one |
| `consent_denied` | the host said no. The same session is then refused `rate_limited` for 60 s |
| `consent_timeout` / `no_prompt_surface` | nobody answered / nobody could be asked |
| `no_indicator_surface` | nothing on this device could show that it is being recorded |
| `start_failed` | the recorder refused or failed; `detail` says which |

A recording ends `requested` (the controller's Stop), `host_stopped` (the host's
Stop: the banner, the tray, the Recordings view, `roomler record stop`),
`gate_revoked`, `session_ended`, or with the recorder's own reasons (`disk_low`,
`max_duration`, …). The device reports every outcome as `rc:recording.activity`.

⚠️ **A fresh prompt id, never the session's.** The session already has an
answered prompt. A decision recorded against its id, or anything derived from
it, must not be able to answer this one: the `ssh` / `ssh-consent` lesson.

⚠️ **Something on screen says "recording" before the first frame.** On Windows
the daemon's own badge is pinned open while recording and reads "Recording,
viewed by …". It is capture-excluded, so it is not in the recording. Everywhere
else the companion's banner reads "Recording your screen for …" and has a
**Stop recording** button that keeps the session. On X11 and macOS that banner
is not capture-excluded and appears in the recording. With no surface at all the
start is refused. There is no unattended exception yet: an unattended host runs
the recorder as SYSTEM/root, which P1e must solve first.

⚠️ **An older companion.** A companion that predates `record` renders an
unknown prompt kind as a remote-control request. So a record prompt carries the
whole question in its detail line, the one field every companion shows as it
is.

Not in P3b yet: the download (`rc:record.list` / `rc:record.get`, resumable)
and the 60 s re-attach after a dropped session (P3b-2), and the viewer's Record
button (P3c).

### Decision and claim, like SSH

| Collection | Written by | What it is |
|---|---|---|
| `remote_audit` | the server | Its **decision**: the grant, with `record_stripped` when RECORD was asked for and refused. Authoritative. |
| `recording_activity` | the device, through `rc:recording.activity` | Its **claims**: a prompt granted, denied or timed out; a recording started, stopped or refused; a download. Each carries a file name, bytes, duration and reason, with text clipped to 256 characters. Never content. 90-day TTL. |

A claim is kept only for a session **of the sending device** whose effective
grant held `RECORD`. The server checks the live session in the hub, or its
stored `remote_sessions` row once it has ended, because a stop can arrive after
the session. Anything else is dropped and logged. So a device cannot write about
another device's session, and it cannot invent recording activity for a session
that was never allowed to record. It is still a claim by a host that may be
compromised: join it on `session_id` with `remote_audit` for the decision.

`GET /api/tenant/{tenant_id}/recording-activity/{agent_id}` reads it, newest
first and paginated. It is gated by `VIEW_REMOTE_AUDIT`, like the session
audit. The device is resolved within the tenant, so a foreign id gets a 404.

## 11. Tests

| Where | What | CI |
|---|---|---|
| `recording::*` unit tests | Annex-B split and parameter sets; the fragmented writer (keyframe cuts, refusals, never overwriting); remux sample order with `moov` first; truncated recovery; audio interleave; the pacer's tick math and FIFO; folder probe, validation, names, OneDrive/UNC; sidecar round trip; the manager's event folding, delete-name rules and listing; the identity probe | `ci.yml` "Test the recorder (FR-85)" (`--lib recording::`) |
| `tests/recorder.rs` | A counter-pattern capture → openh264 recording encoder → MP4 → openh264 decode, reading the counters back (the oracle is proven to discriminate first); display change; disk-low and no-frame refusals; the real `roomlerd record` process: the stop command, stdin EOF, `kill -9` followed by `reconcile_partials`, a **live** partial left alone (red with the lock disabled); and the manager end to end (start into `record_dir`, a second start refused, stop, list, delete), a missed start deadline killing the child (red without the kill), and the SYSTEM/root refusal | same step, `--test recorder` |
| `recording::audio` unit tests (P1c) | 48 kHz passes through exactly (one frame of interpolator latency); mono → both channels; a 44.1 kHz sine resamples to 48 kHz at the same pitch; a positive rate trim consumes faster; the soft clip is linear below the knee, monotonic, never wraps; a silent source still yields one frame per 20 ms; two sources sum; a backlog is cut to the lag keeping the newest; a 0.2 % fast source is held near the lag by the rate correction, never trimmed | "Test the recorder (FR-85)", the `audio` run |
| `tests/recorder.rs`, audio (P1c) | a 440 Hz tone at 44.1 kHz mono plus a microphone that delivers nothing → an Opus track within 80 ms of the video, decoded back at 440 Hz with the right level; the same recording without audio has no audio track (the negative control); `roomlerd record --system-audio` through the real process; a build without `audio` refuses `--microphone` with `audio_unavailable` | both runs of the same step |
| `crates/localapi` | the console-user decision table; a recording verb from an unidentified peer is refused before any handler runs; `ConfigSet record_dir` gated the same way; the verbs round-trip | "Run the remaining crates' unit tests" |
| `crates/agent-core` | `record_dir` set/echo/validate/clear; the live set is exactly `exec_enabled`, `remote_config_enabled`, `record_dir`, `record_remote_enabled`, `record_remote_audio`; `recording_dir` validation incl. a real Windows junction | same |
| `recording::remote` unit tests (P3b) | nothing advertised unless the owner opted in AND a recorder can run, `remote-audio` only on top with an audio build; the prechecks refuse in order and by name; the wire parses (audio off unless asked; a `microphone` field is ignored) and speaks the documented state shape; the controller is told a file name, never a path; `adopt` signals only a change | "Test the recorder (FR-85)" (`--lib recording::`) |
| `tests/control_dc_record.rs` (P3b) | A loopback PeerConnection pair, the PRODUCTION `record` handler and the real recorder child. The owner's switch and the audio gate refuse by name, leave no file and no banner, and are reported to the server; a grant without RECORD gets a refusing channel. No indicator surface means no recording and no running recorder. An auto-granted session records: the banner is up before the file, a second start is `busy`, Stop ends it `requested`, and the sidecar names the controller with no microphone. A host-consented session asks again with a FRESH prompt id (never the session's), the detail line carries the question, a deny holds for a minute (`rate_limited`), and an approval records. The owner's OFF ends it `gate_revoked`, the host's Stop `host_stopped`, the controller going away `session_ended` | same step, `--test control_dc_record` |
| `rc_sessions` (P3b) | the banner's `recording` follows the session, survives a re-announce, and cannot be set on a session the banner does not show | the default `--lib` step |
| `crates/remote_control` | `no_record_key_is_server_pushable_via_desired_config`; `remote_recording_does_not_imply_remote_audio` (equality, an old agent's hello advertises nothing, a newer word is ignored, the wire words are pinned); `rc:recording.activity` owned by `remote` | same |
| `crates/db` (P3a) | `RECORD_REMOTE_SCREEN` is named, inside `ALL`, in no managed row below `ADMINISTRATOR`, and outside `DEFAULT_ADMIN` | same |
| `crates/modules/fleet` (P3a) | `record_grant`'s table: kept only when the controller may AND the device serves it, each refusal with its reason, a grant without RECORD untouched; the hub strips RECORD from the effective grant and names why in `SessionCreated`; a coalesced duplicate repeats the reason | "Run fleet module unit tests" |
| `crates/tests/src/remote_recording_tests.rs` (P3a) | Real servers, WebSockets and MongoDB, one controller connection per request (a second request on one socket coalesces). The owner on an opted-in device keeps RECORD, and on a device that never opted in gets `device_not_opted_in`. A member with `REMOTE_CONTROL` gets `controller_not_allowed`; the same member with `RECORD_REMOTE_SCREEN` keeps it (the positive control). A non-owner `ADMINISTRATOR` keeps it; under break-glass it is stripped. A heartbeat re-announcing caps opts the device in and out without a reconnect, and one without caps changes nothing. Activity is kept for a RECORD session of the sending device only (not for a session without RECORD, not from another device). The route answers RFC 3339 times and refuses a caller without `VIEW_REMOTE_AUDIT` | `integration-tests.yml` |
| `ui/src/__tests__/utils/permissions.spec.ts` (P3a) | the catalogue lists 32 bits, `RECORD_REMOTE_SCREEN` is `2 ** 31` (positive, not `1 << 31`), and mask arithmetic keeps bit 31 | "Frontend checks" |
| `agents/roomler-cli` | `record` verbs parse; lengths and endings read plainly | same |
| `ui/src/__tests__/companion/recordings.spec.ts` | roomler-desktop's REAL `index.html` section and `recordings.js`, in jsdom against a mocked `invoke`: Start greyed out with the reason, the running state, start options, a refusal said, delete only on the second click (red when a single click deletes), the arm expiring, the folder picker saving through `cmd_config_set` and a cancel saving nothing, keyed rows kept in place (red when rows are rebuilt), the last good data kept on a failed refresh, a service with no recorder | "Frontend checks" (`bun run test:unit`) |
| `ui/src/__tests__/companion/recordings.spec.ts` (P3b) | the remote-recording card: absent against a service that predates the gates; computer audio offered only once remote recording is allowed; each toggle saved through `cmd_config_set`; a refused toggle said and not faked; a remote recording's status names who it is for | "Frontend checks" |
| `ui/src/__tests__/companion/viewing.spec.ts` (P3b) | the REAL banner (`panel-viewing.html` / `.js`): who is watching; a RECORDING controller leads, even when another viewer came first (red when the lookup is removed); Stop recording stops the recording and not the session; the notice comes down when the recording ends | "Frontend checks" |
| `agents/roomler-desktop` | the tray's wording and when its item is enabled; only a bare `*.mp4` name is opened; a service without the recorder reads as unsupported; recording keys (incl. both remote gates) are the daemon's to accept; the remote gates come from the listing and are absent on an older service | `ci.yml` "Test the desktop companion (roomler-desktop)", new with P2b. The crate's unit tests ran in NO lane before: the macOS job only `cargo check`s it, and the shared step is `--lib`, which a bin-only crate cannot join |

⚠️ `agents/roomlerd/tests/*.rs` runs only when a step **names** it. Every other
roomlerd test step is `--lib`, which is why `tests/file_dc.rs` has never run in
CI.
