// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, oid::ObjectId};
use serde::{Deserialize, Serialize};

use crate::permissions::Permissions;

// ────────────────────────────────────────────────────────────────────────────
// Agent
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OsKind {
    Linux,
    Macos,
    Windows,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Online,
    Offline,
    Unenrolled,
    Quarantined,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DisplayInfo {
    pub index: u8,
    pub name: String,
    pub width_px: u32,
    pub height_px: u32,
    pub scale: f32,
    pub primary: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AgentCaps {
    pub hw_encoders: Vec<String>,
    pub codecs: Vec<String>,
    pub has_input_permission: bool,
    pub supports_clipboard: bool,
    pub supports_file_transfer: bool,
    pub max_simultaneous_sessions: u8,
    /// Video transport modes the agent supports beyond the default
    /// WebRTC video track. Empty / unset means WebRTC video only
    /// (the legacy default; older agents that don't know about
    /// this field deserialize that way via serde default).
    ///
    /// Known value: `data-channel-vp9-444` — VP9 profile 1
    /// (8-bit 4:4:4) frames over an RTCDataChannel named
    /// `video-bytes`. Bypasses the browser's WebRTC video pipeline
    /// which enforces 4:2:0 across every codec. See
    /// `docs/encoders.md` for the rationale and the wire
    /// format spec.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transports: Vec<String>,
    /// File-DC v2 (0.3.0+) capability list. Replaces the
    /// coarse-grained `supports_file_transfer` bool with explicit
    /// per-feature flags. Recognised values:
    ///
    /// * `upload`   — browser → host file uploads (the v1 default).
    /// * `download` — host → browser single-file downloads.
    /// * `download-folder` — host → browser folder zip streams.
    /// * `browse`   — browser can navigate the host's filesystem
    ///   via `files:dir`. Conditional on the agent's
    ///   `enable_remote_browse` config flag.
    ///
    /// Empty / unset (older agents) deserialises to `[]`; browsers
    /// that see an empty list fall back to `supports_file_transfer`
    /// to determine just upload availability. New browsers that need
    /// download/browse functionality check this list and grey out
    /// the affected toolbar buttons when the capability is missing,
    /// instead of waiting for a 5 s timeout on an unanswered
    /// `files:get` / `files:dir`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// FR-17 — video DATA-CHANNEL wire options the agent supports, as
    /// opposed to which transport it offers (that is `transports`).
    /// Recognised values:
    ///
    /// * `chunk-framing` — every 16 KiB DataChannel message carries an
    ///   8-byte `[frame_seq | chunk_idx | chunk_count]` prefix, so a
    ///   receiver can tell WHICH frame a message belongs to and notice a
    ///   missing one. Without it the stream is only reassemblable because
    ///   the channel is reliable+ordered, which is the property FR-17
    ///   exists to give up: on a lossy relay that ordering costs seconds
    ///   of head-of-line blocking (measured: one frame blocked 10 263 ms).
    ///
    /// Empty / unset (older agents) deserialises to `[]` and the browser
    /// keeps the legacy unframed format. The capability is what makes the
    /// negotiation race-free: the viewer knows before it connects, so
    /// there is never a window where the two ends disagree about how to
    /// parse bytes already in flight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<String>,
    /// P6 — multi-user input capabilities. Recognised values:
    ///
    /// * `arbiter` — the agent runs the InputArbiter (single fenced
    ///   injection worker); the SERVER lifts the P3 single-INPUT-holder
    ///   downgrade for concurrent sessions on agents advertising this.
    /// * `exclusive` — the arbiter supports the exclusive floor mode
    ///   (`rc:control.request` / `rc:control.mode` on the control DC).
    /// * `ghost-cursor` — other sessions' pointers are rebroadcast as
    ///   `cursor:peer` on the cursor DC.
    ///
    /// Empty / unset = a pre-P6 agent → the P3 single-holder rule stays.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<String>,
    /// Host permissions the OS has actually GRANTED this agent, as opposed to
    /// what it was compiled with. Recognised values:
    ///
    /// * `screen-capture` — the agent may capture the screen. On macOS this
    ///   is the Screen Recording TCC grant; elsewhere there is no such gate
    ///   and it is always present.
    /// * `input` — the agent may inject keyboard/mouse events. On macOS this
    ///   is the Accessibility grant.
    /// * `no-gui-session` — a THIRD state, distinct from granted and denied:
    ///   this process is not in a GUI login session (macOS's root
    ///   LaunchDaemon), so capture and input are impossible regardless of any
    ///   grant. Present ALONE — never alongside the two above. Readers must
    ///   treat it as "this device is not a capture target", not as "two
    ///   permissions are missing", or a mesh-only daemon reads as broken and
    ///   sends the operator after a toggle that would change nothing.
    ///
    /// ⚠️ `None` and `Some([])` mean OPPOSITE things and must never be
    /// collapsed: `None` is a pre-rc.454 agent that cannot report (no
    /// information — say nothing), `Some([])` is an agent reporting that it
    /// holds NEITHER permission (a Mac that will show a blank screen and
    /// swallow every click). Same rule as `NetmapPeer.ingress_rules`.
    ///
    /// This exists because macOS never ERRORS on a missing grant: capture
    /// silently returns wallpaper-only frames and injected input is silently
    /// dropped, so without this the product's only symptom is "it doesn't
    /// work" with a clean log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<String>>,
    /// Multi-org capabilities. Recognised values:
    ///
    /// * `join` — the agent honours `rc:agent.join_org`: it enrolls itself
    ///   into an ADDITIONAL org on the same server from a pushed enrollment
    ///   token and brings that org's supervisor up without a restart.
    ///
    /// Empty / unset = a pre-rc.310 agent. The admin UI greys out
    /// "Add to another organization" for those; the server refuses to mint a
    /// token it knows can't be consumed, rather than pushing a message that
    /// lands in the agent's unknown-variant debug branch and vanishes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub multi_org: Vec<String>,
    /// VP9 chroma format the agent will emit on the
    /// `data-channel-vp9-444` transport. Values: `"yuv444"` (default,
    /// current behaviour, VP9 profile 1) for sharpest text via
    /// ClearType chroma preservation, or `"yuv420"` (VP9 profile 0)
    /// for ~1.5× lower bandwidth at the cost of slight chroma loss
    /// on small Windows ClearType text.
    ///
    /// rc.61 — added so the browser-side `rc-vp9-444-worker.ts` can
    /// pick the right codec string (`vp09.01.10.08` vs `vp09.00.10.08`)
    /// when configuring its `VideoDecoder`. Mismatch leaves the canvas
    /// blank. Empty / older agents deserialise to `""`; browsers treat
    /// the empty value as `"yuv444"` for backward compat.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vp9_chroma: String,
    /// P7 (2026-08-20) — chroma formats the agent's HEVC encoder can emit
    /// on the `data-channel-hevc` transport. `"yuv420"` (Main profile —
    /// every HEVC host) and `"yuv444"` (Rext profile — hevc_nvenc only;
    /// NVENC supports HEVC 4:4:4 since Maxwell-gen2). The browser offers
    /// its "HEVC · crisp text (4:4:4)" picker entry only when this list
    /// contains `"yuv444"` AND its own WebCodecs Rext decode probe passes
    /// (Chrome ≥137 + NVIDIA driver ≥572.16, or Intel Gen11+ ≥117 — there
    /// is NO software HEVC fallback in Chrome). Empty / unset (older
    /// agents, non-nvenc hosts) → the picker entry stays hidden and
    /// sessions run Main-profile 4:2:0 exactly as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hevc_chroma: Vec<String>,
    /// Audio codecs the agent can stream on the WebRTC audio track
    /// (system / desktop audio, opt-in per session). Empty / unset
    /// (older agents, or agents built without the `audio` feature)
    /// means "no audio track offered" — the browser must not request
    /// `audio_enabled`. Known value: `"opus"` (audio/opus, 48 kHz
    /// stereo, PT 111 — the WebRTC default). Populated by the agent's
    /// caps builder only when the `audio` Cargo feature is compiled in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub audio: Vec<String>,
    /// Remote app selection & launch on virtual-desktop hosts. Present +
    /// non-empty only when the agent can manage a desktop (Linux
    /// virtual-desktop mode) AND `virtual_desktop_apps.enabled`. Known
    /// values: `"list"`, `"focus"`, `"launch"`. Empty / unset (older
    /// agents, non-VD hosts) → the browser hides the Apps menu.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
    /// Fleet-RPC capabilities. Recognised values:
    ///
    /// * `exec` — the agent honours `rc:rpc.exec` / `rc:rpc.cancel`: it runs
    ///   one bounded shell command and replies `rc:rpc.result`.
    /// * `originate` — the agent honours `rc:rpc.response`, i.e. its LocalAPI
    ///   can originate an exec against ANOTHER device (the `roomler exec`
    ///   CLI path).
    /// * `ssh` — the agent honours `rc:ssh.grant`: it serves roomler SSH on
    ///   its overlay address and will admit the granted key.
    /// * `ssh-originate` — the agent honours `rc:ssh.response`, i.e. its
    ///   LocalAPI can originate an SSH session against ANOTHER device (the
    ///   `roomler ssh` CLI path).
    ///
    /// Empty / unset = a pre-fleet-RPC agent. Same rule as [`Self::multi_org`]:
    /// the server refuses the request with `412` rather than pushing a message
    /// that lands in the agent's unknown-variant debug branch and vanishes,
    /// leaving the caller to hang until its deadline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rpc: Vec<String>,
    /// Clipboard-DC protocol-v2 capability list. Extends the coarse
    /// `supports_clipboard` bool (kept for older browsers) with
    /// per-feature flags. Recognised values:
    ///
    /// * `"ack"`    — the agent replies `clipboard:write-ack` to
    ///   `clipboard:write` / `clipboard:write-chunk` payloads that
    ///   carry an `id`, letting the browser gate its deferred Ctrl+V
    ///   keystroke on the host clipboard actually being written.
    /// * `"events"` — the agent accepts `clipboard:subscribe` and
    ///   pushes unsolicited `clipboard:event` / `clipboard:img-begin`
    ///   messages when the host clipboard changes (host→browser
    ///   auto-sync). Change *watching* for images is Windows-only;
    ///   text watching works everywhere arboard does.
    /// * `"images"` — the agent understands PNG image payloads on the
    ///   clipboard DC in both directions (`clipboard:img-begin` /
    ///   binary frames / `clipboard:img-end`, and `accept:["image"]`
    ///   on `clipboard:read`).
    ///
    /// Empty / unset (older agents, builds without the `clipboard`
    /// feature) deserialises to `[]`; browsers then fall back to the
    /// v1 button-driven text-only flow.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clipboard: Vec<String>,
    /// Keyboard-layout integration (Windows hosts with input
    /// injection). Recognised values:
    ///
    /// * `"report"` — the agent pushes `rc:layout` snapshots (active +
    ///   installed layouts) over the control DC.
    /// * `"set"` — the agent accepts `rc:layout.set {hkl}` manual
    ///   switches from the viewer's layout picker.
    ///
    /// Empty / unset (older agents, non-Windows hosts, builds without
    /// `enigo-input`) → the browser hides all layout UI. The per-char
    /// auto-switch itself is agent-local and needs no browser
    /// involvement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layout: Vec<String>,
    /// FR-77 — every **cell** (codec × chroma format) this host can produce,
    /// one entry per *encoder* (codec × backend) the start-up probe actually
    /// OPENED, with the chroma formats that open succeeded for. Additive: an
    /// agent older than FR-77 sends nothing and a viewer derives cells from
    /// the legacy fields (`hw_encoders`, `transports`, `hevc_chroma`,
    /// `vp9_chroma`), which keep being filled forever — renaming or dropping
    /// them would strand every deployed agent.
    ///
    /// The wire stays strings ([`VideoCell`]); both sides go through
    /// [`VideoCodec`], [`VideoBackend`] and [`ChromaFormat`] so the vocabulary
    /// lives in one place. Unknown codecs, backends or chroma formats from a
    /// NEWER agent are ignored by an older reader, never an error — the
    /// additive-list rule the `rpc` verbs follow.
    ///
    /// `hw` is VERIFIED, never assumed: NVENC, AMF, VideoToolbox and VAAPI are
    /// hardware by construction; a QSV open is hardware only on the oneVPL
    /// build, whose dispatcher filters `MFX_IMPL_TYPE_HARDWARE` and never
    /// enumerates the CPU runtime; Media Foundation reports what its cascade
    /// landed on; `openh264` and `libvpx` are software.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video_cells: Vec<VideoCell>,
    /// FR-77 — wall-clock milliseconds the start-up capability probe took
    /// (child spawn to parsed result), so the fleet can MEASURE what opening
    /// every cell costs instead of assuming it. `None` when the probe child did
    /// not come back (the caps are then the driver-free fallback) or from an
    /// agent older than FR-77.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_ms: Option<u32>,
}

/// FR-77 — one encoder (codec × backend) and the chroma formats it produced
/// when the probe opened it. See [`AgentCaps::video_cells`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct VideoCell {
    /// A [`VideoCodec`] wire name: `h264` · `hevc` · `av1` · `vp9`.
    pub codec: String,
    /// A [`VideoBackend`] wire name: `nvenc` · `qsv` · `amf` · `videotoolbox`
    /// · `vaapi` · `mf` · `openh264` · `libvpx`.
    pub backend: String,
    /// [`ChromaFormat`] wire names the open succeeded for, `yuv420` first.
    #[serde(default)]
    pub chroma: Vec<String>,
    /// Verified hardware encode (see [`AgentCaps::video_cells`]).
    #[serde(default)]
    pub hw: bool,
}

/// FR-77 — a codec the agent can produce. Same contract as [`RpcCap`]: the
/// wire is a string, the vocabulary is this enum, unknown strings are ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    Vp9,
}

impl VideoCodec {
    /// Exactly what crosses the wire. ⚠️ A compatibility surface — see
    /// [`RpcCap::wire`].
    pub const fn wire(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
            Self::Vp9 => "vp9",
        }
    }

    pub const ALL: [VideoCodec; 4] = [Self::H264, Self::Hevc, Self::Av1, Self::Vp9];

    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.wire() == s)
    }
}

/// FR-77 — the implementation that produced a codec's bitstream. A property of
/// the HOST, discovered at runtime, never chosen by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoBackend {
    Nvenc,
    Qsv,
    Amf,
    VideoToolbox,
    Vaapi,
    /// The agent's native Media Foundation module (Windows), not FFmpeg's
    /// `*_mf` wrappers, which are deliberately not built.
    MediaFoundation,
    Openh264,
    Libvpx,
}

impl VideoBackend {
    /// Exactly what crosses the wire. ⚠️ A compatibility surface.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Nvenc => "nvenc",
            Self::Qsv => "qsv",
            Self::Amf => "amf",
            Self::VideoToolbox => "videotoolbox",
            Self::Vaapi => "vaapi",
            Self::MediaFoundation => "mf",
            Self::Openh264 => "openh264",
            Self::Libvpx => "libvpx",
        }
    }

    pub const ALL: [VideoBackend; 8] = [
        Self::Nvenc,
        Self::Qsv,
        Self::Amf,
        Self::VideoToolbox,
        Self::Vaapi,
        Self::MediaFoundation,
        Self::Openh264,
        Self::Libvpx,
    ];

    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.wire() == s)
    }

    /// Split an FFmpeg encoder name (`hevc_nvenc`, `h264_videotoolbox`) into
    /// its codec and backend. `None` for a name this build does not know —
    /// which is how a NEW backend fails loudly in a test instead of being
    /// advertised under a name nobody parses.
    pub fn from_ffmpeg_name(name: &str) -> Option<(VideoCodec, VideoBackend)> {
        let (codec, backend) = name.split_once('_')?;
        let codec = VideoCodec::from_wire(codec)?;
        let backend = match backend {
            "nvenc" => Self::Nvenc,
            "qsv" => Self::Qsv,
            "amf" => Self::Amf,
            "videotoolbox" => Self::VideoToolbox,
            "vaapi" => Self::Vaapi,
            _ => return None,
        };
        Some((codec, backend))
    }
}

/// FR-77 — how much colour resolution an encoded stream keeps. A property of
/// the STREAM that both the encoder and the decoder must support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromaFormat {
    /// Colour at quarter resolution — the video default, every backend.
    Yuv420,
    /// Full colour resolution — what keeps coloured text and thin lines sharp.
    Yuv444,
}

impl ChromaFormat {
    /// Exactly what crosses the wire — the same spellings `vp9_chroma`,
    /// `hevc_chroma` and `chroma_pref` have carried since rc.61.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Yuv420 => "yuv420",
            Self::Yuv444 => "yuv444",
        }
    }

    pub const ALL: [ChromaFormat; 2] = [Self::Yuv420, Self::Yuv444];

    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.wire() == s)
    }
}

/// FR-77 — a [`VideoCell`] read back through the vocabulary. Anything the
/// reader does not know is simply not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedCell {
    pub codec: VideoCodec,
    pub backend: VideoBackend,
    pub chroma: Vec<ChromaFormat>,
    pub hw: bool,
}

impl VideoCell {
    /// Build a wire cell from the vocabulary — the producer side.
    pub fn new(
        codec: VideoCodec,
        backend: VideoBackend,
        chroma: &[ChromaFormat],
        hw: bool,
    ) -> Self {
        Self {
            codec: codec.wire().to_string(),
            backend: backend.wire().to_string(),
            chroma: chroma.iter().map(|c| c.wire().to_string()).collect(),
            hw,
        }
    }

    /// Parse a wire cell. `None` when the codec or backend is unknown to this
    /// build (a NEWER agent), and unknown chroma strings are dropped — the
    /// additive-list rule.
    pub fn typed(&self) -> Option<TypedCell> {
        let codec = VideoCodec::from_wire(&self.codec)?;
        let backend = VideoBackend::from_wire(&self.backend)?;
        let chroma = self
            .chroma
            .iter()
            .filter_map(|c| ChromaFormat::from_wire(c))
            .collect();
        Some(TypedCell {
            codec,
            backend,
            chroma,
            hw: self.hw,
        })
    }
}

/// A verb in [`AgentCaps::rpc`] — "this agent understands this frame".
///
/// The WIRE stays `Vec<String>` on purpose. Agents in the field span many
/// releases, and a typed wire format would strand every one of them; the point
/// of this enum is not to change the protocol but to make sure **both sides go
/// through one vocabulary**. Before it, the agent wrote four string literals
/// and three server call sites compared against four more, so a typo could
/// only be caught by a device silently looking unsupported — the exact
/// failure the capability list exists to prevent.
///
/// Advertising a verb says only that the agent UNDERSTANDS the frame. The org
/// kill-switch, the device's policy and the agent-local config key all still
/// have to say yes before anything happens.
///
/// **Adding one:** add the variant, then its arm in [`Self::wire`] (the match
/// is exhaustive, so the compiler makes you), then the entry in [`Self::ALL`]
/// (the `all_entries_are_unique_and_round_trip` test makes you). Consumers then
/// name it by variant and can no longer misspell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RpcCap {
    /// `rc:rpc.exec` — run a bounded command (Fleet RPC).
    Exec,
    /// This device's LocalAPI may ORIGINATE a session against another device
    /// (`roomler ssh` / `roomler exec` from the device itself).
    Originate,
    /// `rc:ssh.grant` — the agent runs an SSH server and can redeem a grant.
    /// Gated on the `ssh-server` build feature, not merely on the version.
    Ssh,
    /// The agent HONOURS `SshPolicy.consent_mode`.
    ///
    /// Deliberately distinct from [`Self::Ssh`]: agents rc.419 and earlier
    /// advertise `ssh` while destructuring `consent_mode` away, so the server
    /// needs to ask "does this device actually do the thing?" rather than
    /// "does it know the feature exists". Expect that distinction to recur —
    /// it is why a verb is finer-grained than a version check.
    SshConsent,
    /// `rc:agent.config` — the agent understands a pushed desired-config and
    /// will reconcile against it (`docs/remote-config.md`).
    ///
    /// ⚠️ Advertising this says only that the frame is UNDERSTOOD, never that
    /// it will be obeyed. A device still refuses unless it has opted in with
    /// `remote_config_enabled`, and a secondary org's push is ignored
    /// regardless. The server needs the distinction because an older agent
    /// drops an unknown frame SILENTLY — the parse-error arm logs at `debug!`
    /// and carries on — so without this verb a dashboard would show a change
    /// that simply evaporated.
    Config,
    /// `rc:agent.config_status` — the agent reports back what it DID with a
    /// pushed desired-config.
    ///
    /// ⚠️ Distinct from [`Self::Config`] for the reason [`Self::SshConsent`] is
    /// distinct from [`Self::Ssh`], and this is the second time that shape has
    /// occurred exactly as `SshConsent`'s doc predicted: agents rc.457 and
    /// rc.458 shipped `config` — they apply a pushed config and say NOTHING
    /// about it. Folding the report into `config` would tell the dashboard
    /// "this device reports its outcome" about a fleet that does not, so a
    /// device that refused would be indistinguishable from one still thinking
    /// about it. A verb is finer-grained than a version check.
    ///
    /// ⚠️ `config` is a PREFIX of `config-report` and they mean different
    /// things, so matching must stay equality — see
    /// [`AgentCaps::has_rpc`] and the test that locks it.
    ConfigReport,
    /// FR-19 — this device runs an **org-relay server**: it has bound
    /// `relay_server_port`, answers probes, and (P2c+) forwards ciphertext
    /// between bound members. The server never installs a relay session on a
    /// device that does not advertise this.
    ///
    /// ⚠️ Equality-matched like every verb here. There is no `relay` verb
    /// today, and if one is ever added it must not be matched by prefix
    /// against this one — the `ssh` / `ssh-consent` trap, locked by test.
    RelayServer,
    /// FR-40 — `rc:agent.key_rotate`: the agent will mint a fresh overlay
    /// (WireGuard) key on order, persist it, report back and re-join under
    /// it (`docs/fr/FR-40-overlay-key-rotation.md`).
    ///
    /// ⚠️ Unlike `rc:agent.update`, the order must NOT be sent blind. The
    /// dashboard shows an operator a rotation in flight, so a pre-feature
    /// agent dropping the unknown tag in its `debug!` branch would be a lie
    /// on a screen — and a lie about a SECURITY action, which is worse than
    /// the config case this verb copies. Gate on it and say "device too old".
    ///
    /// Only advertised by builds with an overlay surface (`overlay-l3` /
    /// `overlay-netstack`): a build with no key has nothing to rotate.
    KeyRotate,
}

impl RpcCap {
    /// Exactly what crosses the wire.
    ///
    /// ⚠️ These strings are a COMPATIBILITY SURFACE, not an implementation
    /// detail: changing one silently un-advertises the feature on every agent
    /// already deployed, which reads server-side as "this device does not
    /// support it" rather than as an error. Locked by test.
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Originate => "originate",
            Self::Ssh => "ssh",
            Self::SshConsent => "ssh-consent",
            Self::Config => "config",
            Self::ConfigReport => "config-report",
            Self::RelayServer => "relay-server",
            Self::KeyRotate => "key-rotate",
        }
    }

    /// Every verb THIS build knows about.
    pub const ALL: [RpcCap; 8] = [
        Self::Exec,
        Self::Originate,
        Self::Ssh,
        Self::SshConsent,
        Self::Config,
        Self::ConfigReport,
        Self::RelayServer,
        Self::KeyRotate,
    ];

    /// Parse a wire verb. `None` for anything unrecognised — see
    /// [`AgentCaps::has_rpc`] for why that is not an error.
    pub fn from_wire(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.wire() == s)
    }
}

impl AgentCaps {
    /// Does this agent advertise `cap`?
    ///
    /// ⚠️ Unrecognised verbs are IGNORED rather than rejected. A NEWER agent
    /// may advertise something this server has never heard of, and the whole
    /// point of an additive string list is that an older reader keeps working
    /// — so "I don't know that verb" must mean "not one I gate on", never an
    /// error.
    pub fn has_rpc(&self, cap: RpcCap) -> bool {
        self.rpc.iter().any(|v| v == cap.wire())
    }

    /// FR-77 — the cells this reader can name. Entries from a newer agent
    /// whose codec or backend this build does not know are skipped, and a
    /// pre-FR-77 agent yields an empty list — callers then fall back to the
    /// legacy fields, never to an error.
    pub fn typed_cells(&self) -> Vec<TypedCell> {
        self.video_cells
            .iter()
            .filter_map(VideoCell::typed)
            .collect()
    }

    /// FR-77 — can this host produce `codec` in `chroma` on ANY backend?
    /// Reads only [`Self::video_cells`]; an agent that predates them answers
    /// `false` here and is judged by the legacy fields instead.
    pub fn has_cell(&self, codec: VideoCodec, chroma: ChromaFormat) -> bool {
        self.typed_cells()
            .iter()
            .any(|c| c.codec == codec && c.chroma.contains(&chroma))
    }
}

/// How consent is obtained before a controller may drive a device. Resolved
/// server-side per session from the device's [`AccessPolicy::consent_mode`]
/// (with `Prompt` — attended — as the system default), then carried to the agent
/// in `ServerMsg::Request` as a directive the agent obeys. Self-control
/// (`controller == owner_user_id`) short-circuits to `Auto` in the API gate
/// unless the device set [`AccessPolicy::prompt_owner`].
///
/// ⚠️ The directive is a CEILING, not a floor. A device that set
/// `auto_grant_session = false` locally always prompts, even under `Auto` —
/// the agent resolves the two with `consent::strictest_of`. Same rule as
/// exec's and SSH's gate 4: the device's own refusal survives the server.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConsentMode {
    /// Unattended: grant immediately, no prompt. For self-owned / kiosk / server
    /// devices explicitly marked unattended.
    Auto,
    /// Attended (the default): the controlled host prompts (tray / CLI) and the
    /// person there must approve within the timeout.
    #[default]
    Prompt,
    /// Email an approve-link to the device owner; the session waits (Phase 4).
    Email,
    /// Push an in-app consent card to the device owner (Phase 4).
    Push,
    /// Prompt the host AND email the owner, in parallel — first answer wins.
    ///
    /// Not a sequential fallback, despite the name: the server emails the
    /// approve-link the moment the session is requested, at the same time as
    /// the agent raises its on-host prompt. What IS sequential is the two
    /// windows — the host prompt closes after
    /// [`crate::consent::HOST_PROMPT_TIMEOUT`] while the emailed link stays
    /// live for the full [`crate::consent::ASYNC_CONSENT_TIMEOUT`], so a host
    /// nobody answers hands over to the owner instead of ending the session.
    /// An explicit host *Deny* does end it: the person at the machine said no.
    PromptThenEmail,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AccessPolicy {
    /// How consent is obtained for a non-owner controller. `None` = inherit the
    /// system default ([`ConsentMode::Prompt`] — attended). Set per device by a
    /// `MANAGE_AGENTS` admin. (Replaces the legacy `require_consent` bool; old
    /// rows carrying that field deserialize to `None` → attended, the safe
    /// default.)
    #[serde(default)]
    pub consent_mode: Option<ConsentMode>,
    /// FR-27 — apply [`Self::consent_mode`] to the device's OWNER too.
    ///
    /// `None`/`false` (the default, and every pre-FR-27 row) keeps the
    /// historical shortcut: controlling your own device auto-consents, because
    /// unattended access to your own headless boxes is the common case and
    /// prompting them would ask a machine nobody is sitting at.
    ///
    /// That shortcut is also why the picker looked broken. It is applied in
    /// `resolve_session_authz` BEFORE the policy is read, so on a fleet where
    /// one person owns every device, `consent_mode` had no observable effect
    /// whatsoever — and nothing on screen said so. `true` here makes the mode
    /// authoritative for the owner as well, which is both the honest behaviour
    /// for a shared workstation and the only way to field-test the attended
    /// modes without a second account.
    #[serde(default)]
    pub prompt_owner: Option<bool>,
    #[serde(default)]
    pub allowed_role_ids: Vec<ObjectId>,
    #[serde(default)]
    pub allowed_user_ids: Vec<ObjectId>,
    pub auto_terminate_idle_minutes: Option<u32>,
    /// P6 — multi-user input arbitration mode. `None` = the system default
    /// ([`InputMode::Free`] — free-for-all with agent-side fencing). Old rows
    /// deserialize to `None` via the default.
    #[serde(default)]
    pub input_mode: Option<InputMode>,
}

impl AccessPolicy {
    /// Effective consent mode for a NON-owner controller: the per-device mode,
    /// or the system default (`Prompt` = attended) when unset.
    pub fn effective_consent_mode(&self) -> ConsentMode {
        self.consent_mode.unwrap_or(ConsentMode::Prompt)
    }

    /// FR-27 — the mode for a controller who IS the device owner. `Auto` unless
    /// the device opted into being asked ([`Self::prompt_owner`]).
    ///
    /// Split from [`Self::effective_consent_mode`] rather than folded into it
    /// so the owner shortcut is a named, greppable decision instead of an
    /// `if` buried in the API gate — which is how it stayed invisible.
    pub fn owner_consent_mode(&self) -> ConsentMode {
        if self.prompt_owner.unwrap_or(false) {
            self.effective_consent_mode()
        } else {
            ConsentMode::Auto
        }
    }
}

/// P6 — multi-user input arbitration mode for a device. `Free` (the default)
/// lets every INPUT-granted session inject, serialized + modifier-fenced by
/// the agent's InputArbiter; `Exclusive` funnels input through one explicit
/// floor holder (request/grant, idle takeover). Set per device via
/// `AccessPolicy.input_mode`; toggleable in-session on the control DC.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    #[default]
    Free,
    Exclusive,
}

// ────────────────────────────────────────────────────────────────────────────
// Fleet RPC (remote command execution)
// ────────────────────────────────────────────────────────────────────────────

/// Execution bounds. The server clamps an incoming request to these before it
/// reaches the wire; the agent enforces them again on its own side, so a
/// forged or replayed frame can't ask for an unbounded run.
pub mod exec_limits {
    /// Wall-clock budget when the caller doesn't specify one.
    pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
    /// Hard ceiling. Longer work belongs in a job the command kicks off, not
    /// in a request a caller is blocked on.
    pub const MAX_TIMEOUT_MS: u64 = 300_000;
    /// Combined stdout+stderr ceiling when the caller doesn't specify one.
    pub const DEFAULT_MAX_OUTPUT_BYTES: u64 = 256 * 1024;
    /// Hard ceiling on combined output.
    pub const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;
    /// Concurrent commands one device will run. Beyond this the agent
    /// refuses rather than queues — a caller blocked on a deadline would
    /// rather get a fast "busy" than a slow timeout.
    pub const MAX_CONCURRENT_PER_AGENT: usize = 4;
    /// Requests per minute per (caller, device) pair.
    pub const RATE_LIMIT_PER_MINUTE: u32 = 30;

    /// Clamp a caller-supplied timeout into range, substituting the default
    /// for `None`/0.
    pub fn clamp_timeout_ms(requested: u64) -> u64 {
        match requested {
            0 => DEFAULT_TIMEOUT_MS,
            n => n.min(MAX_TIMEOUT_MS),
        }
    }

    /// Clamp a caller-supplied output ceiling into range, substituting the
    /// default for `None`/0.
    pub fn clamp_output_bytes(requested: u64) -> u64 {
        match requested {
            0 => DEFAULT_MAX_OUTPUT_BYTES,
            n => n.min(MAX_OUTPUT_BYTES),
        }
    }
}

/// Whether a device accepts remote command execution at all.
///
/// Default `Off` — every existing row deserialises to it, so enabling the
/// feature can never retroactively open a device.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecMode {
    /// Refuse every exec request. The default.
    #[default]
    Off,
    /// Accept exec from callers that clear every other gate.
    On,
}

/// Per-device remote-execution policy — gate 3 of four (see
/// `docs/fleet-rpc.md`). Deliberately NOT folded into [`AccessPolicy`]:
/// that grants screen-view, and "may watch your screen" must never be the
/// same checkbox as "may run a root shell".
///
/// ⚠️ On a perMachine Windows install the daemon runs as **SYSTEM**, and on a
/// systemd host as **root** — so `ExecMode::On` is root-equivalent by
/// construction. That is required for the diagnostics this exists for
/// (`Get-NetFirewallRule`, `netsh`, route tables, service state) and is stated
/// in the admin UI's opt-in copy rather than buried here.
/// `PartialEq` so a caller can ask "is this indistinguishable from the
/// untouched default?" — the API listing needs exactly that question to avoid
/// reporting a policy nobody set (see `AgentResponse::exec_policy`).
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct ExecPolicy {
    /// Does this device accept exec requests? Gate 3.
    #[serde(default)]
    pub mode: ExecMode,
    /// May this device *originate* exec against other devices (the
    /// `roomler exec` CLI path, where the daemon's own agent WS carries the
    /// request)? Default `false` — without this, compromising any enrolled
    /// laptop would inherit its owner's exec rights across the whole fleet.
    #[serde(default)]
    pub can_originate: bool,
    /// Restrict callers to these users. Empty = no user restriction (the
    /// permission bit + the other gates still apply).
    #[serde(default)]
    pub allowed_user_ids: Vec<ObjectId>,
    /// Restrict callers to holders of these roles. Empty = no role
    /// restriction.
    #[serde(default)]
    pub allowed_role_ids: Vec<ObjectId>,
    /// How consent is obtained before a command runs. `None` = the safe
    /// default ([`ConsentMode::Prompt`]); unattended servers set `Auto`.
    /// Only `Auto` and `Prompt` are honoured — the email/push variants are
    /// session-shaped and resolve to `Prompt`.
    #[serde(default)]
    pub consent_mode: Option<ConsentMode>,
    /// Shells the device will accept, e.g. `["pwsh", "powershell"]`. Empty =
    /// every shell the host supports.
    #[serde(default)]
    pub shells: Vec<String>,
}

impl ExecPolicy {
    /// Effective consent mode. Unset ⇒ [`ConsentMode::Prompt`] (attended);
    /// the session-shaped variants collapse to `Prompt` because there is no
    /// exec equivalent of an approve-link flow.
    pub fn effective_consent_mode(&self) -> ConsentMode {
        match self.consent_mode {
            Some(ConsentMode::Auto) => ConsentMode::Auto,
            _ => ConsentMode::Prompt,
        }
    }

    /// The shell an empty / `auto` request resolves to on `os`.
    ///
    /// The server resolves BEFORE checking [`Self::allows_shell`], so the
    /// allowlist is compared against the shell that will actually run. Without
    /// this, a device allowing `["powershell"]` refuses the default-shell
    /// request that would have become powershell — which is what
    /// `roomler exec <device> -- …` and every `roomler diag` bundle sends.
    /// Field-caught 2026-08-06: it made `diag` unusable on every device with a
    /// narrowed allowlist.
    ///
    /// MUST stay in lockstep with the agent's `exec::resolve_shell`, or the
    /// policy check and the execution would disagree about what ran.
    pub fn default_shell_for(os: OsKind) -> &'static str {
        match os {
            OsKind::Windows => "powershell",
            _ => "bash",
        }
    }

    /// Normalise a requested shell: empty / `auto` become the host default.
    pub fn resolve_shell(requested: &str, os: OsKind) -> String {
        let t = requested.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("auto") {
            Self::default_shell_for(os).to_string()
        } else {
            t.to_string()
        }
    }

    /// Is `shell` permitted by this policy? An empty list allows everything
    /// the host itself supports. Callers pass an already-resolved name (see
    /// [`Self::resolve_shell`]) — never a bare `""`.
    pub fn allows_shell(&self, shell: &str) -> bool {
        self.shells.is_empty() || self.shells.iter().any(|s| s.eq_ignore_ascii_case(shell))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Agent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// The user who "owns" this device — consent for a non-self controller can
    /// route to them (email/push), and a controller equal to the owner
    /// self-controls (no external allowlist needed). Set to `enrolled_by` at
    /// enrollment; reassignable by a `MANAGE_AGENTS` admin.
    pub owner_user_id: ObjectId,
    /// The user whose enrollment token created this agent (audit; the initial
    /// `owner_user_id`). `#[serde(default)]` → older rows deserialize to `None`.
    #[serde(default)]
    pub enrolled_by: Option<ObjectId>,
    pub name: String,
    /// Friendly label an admin sets purely for display. Never propagates —
    /// `name` is what the overlay/MagicDNS label derives from. `None` = the
    /// UI shows `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Free-form admin labels for fleet filtering/search.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// True once an admin renamed this device. `rehydrate` (re-enroll) then
    /// stops overwriting `name` with the machine-reported one — without this
    /// flag the next re-enroll silently reverted every admin rename.
    #[serde(default)]
    pub name_admin_set: bool,
    pub machine_id: String,
    /// FR-51 — this device declared AT ENROLLMENT that it is temporary: the
    /// reaper may remove it (row, overlay lease, address, MagicDNS name) once
    /// it has been silent past its TTL, and removal is a HARD delete — an
    /// ephemeral row must not tombstone, because the `(tenant_id, machine_id)`
    /// unique index is not partial on `deleted_at`, so a tombstone would
    /// reserve its (random, never-reused) machine_id forever.
    ///
    /// Set only from the enrollment credential (FR-51 P2), never from the
    /// enrollment request body and never editable afterwards: a device that
    /// could declare itself permanent would evade the reaper, and a permanent
    /// device that could be flipped ephemeral would be scheduled for silent
    /// deletion. `#[serde(default)]` → every pre-FR-51 row deserialises to
    /// permanent, so enabling the reaper can never touch an existing fleet.
    #[serde(default)]
    pub ephemeral: bool,
    /// FR-51 — per-device inactivity deadline override, in seconds. `None` =
    /// the server default (`rc.ephemeral_default_ttl_secs`). Clamped to the
    /// 60 s floor at READ time by the reaper, not at write time, so a bad
    /// stored value can never disable the clamp. Meaningless (and absent)
    /// on a permanent row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_ttl_secs: Option<u64>,
    /// FR-51 P2 — the [`EnrollmentKey`] this device was minted by, when it
    /// was. Half of the "which key created this device" chain; the half that
    /// survives the reap is the [`EnrollmentKeyUse`] row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enroll_key_id: Option<ObjectId>,
    pub os: OsKind,
    pub agent_version: String,
    /// FR-27 — the version of the `roomler-desktop` companion INSTALLED on this
    /// host, reported on the heartbeat. `None` means one of three different
    /// things and the grid must not flatten them: a pre-FR-27 agent that never
    /// reports the field, a host with no companion installed, or a probe that
    /// could not read one.
    ///
    /// It exists because the daemon and the companion update through DIFFERENT
    /// mechanisms on every platform — Windows: the daemon side-loads the EXE
    /// (`companion::refresh_if_stale`), macOS: the `.pkg` carries
    /// `/Applications/Roomler.app`, Linux: a separate `roomler-desktop` .deb
    /// that apt owns — so "Update all" moving the daemon says nothing about the
    /// companion, and until now nothing on screen could tell you it had been
    /// left behind. That was the operator's report, not a hypothetical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub companion_version: Option<String>,
    /// P6 — OpenSSH public half of the device's SSH host key, as reported on
    /// its last hello. Published so a caller can verify what it dialled
    /// instead of trusting it on first use.
    ///
    /// Empty = the device stores no host key (never SSH-enabled, or a build
    /// without `ssh-server`), which is NOT "any key is acceptable" — it means
    /// the device cannot prove itself, and a client that cares should refuse.
    ///
    /// ⚠️ The converse does NOT hold. `ssh_host_key` stays in the config after
    /// SSH is switched back off, so a NON-empty value here does not mean the
    /// device is accepting sessions (field-checked: fleet-host-1, `ssh_enabled =
    /// false`, still publishing its P4a key). Whether a session can be opened
    /// is the gate chain's answer, never this field's.
    /// Refreshed from every hello rather than written once, so rotating the
    /// key on the device is enough to move the fleet view with it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh_host_pubkey: String,
    pub agent_token_hash: String,
    pub status: AgentStatus,
    pub last_seen_at: DateTime,
    /// P4 — the presence value (`online`/`stale`/`offline`) most recently
    /// BROADCAST as a `device:presence` event. A cluster-wide transition
    /// ledger: every emit path (register, teardown, staleness sweep, on any
    /// pod) does a CAS on this field and fans out only when it actually
    /// changed, so a transition is announced exactly once no matter how many
    /// pods observe it. `None` on pre-P4 rows / fresh enrollments — the first
    /// observed state always emits. NOT the read-path truth (that stays the
    /// live hub + Redis directory + heartbeat derivation).
    #[serde(default)]
    pub last_presence: Option<String>,
    /// C4 stage 2 — the agent's standing warm TURN allocation's relayed
    /// transport address (`worker-ip:port`), refreshed by every heartbeat
    /// while a leg is live and `$unset` while none is. Stored PAIR-LESS so
    /// a peer whose pair to this agent died can be handed a dial target
    /// without a coordination round-trip through this agent's (possibly
    /// captured) control WS. `None` on pre-stage-2 rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_relay_endpoint: Option<String>,
    #[serde(default)]
    pub displays: Vec<DisplayInfo>,
    #[serde(default)]
    pub capabilities: AgentCaps,
    #[serde(default)]
    pub access_policy: AccessPolicy,
    /// Fleet-RPC policy for this device (gate 3). `#[serde(default)]` → every
    /// pre-feature row deserialises to [`ExecMode::Off`].
    #[serde(default)]
    pub exec_policy: ExecPolicy,
    /// Roomler-SSH policy for this device (gate 3). Same shape and the same
    /// closed default as [`Self::exec_policy`] — every pre-feature row
    /// deserialises to [`SshMode::Off`].
    #[serde(default)]
    pub ssh_policy: SshPolicy,
    /// FR-19 gate 3 — org-relay approval for this device. Same closed default
    /// as the two above: every pre-FR-19 row deserialises to "not approved".
    #[serde(default)]
    pub peer_relay_policy: PeerRelayPolicy,
    /// Device config an operator has ASKED for, reconciled by the agent when
    /// it next connects (`docs/remote-config.md` step 2).
    ///
    /// Desired state, not applied state: writing here records an intent, and
    /// the device remains free to refuse it — a device without
    /// `remote_config_enabled` ignores this entirely, which is the property
    /// that keeps `exec_enabled`/`ssh_enabled` refusable by a compromised
    /// server. Never read this to decide whether a device HAS exec or SSH on;
    /// the device's own heartbeat is the only truth for that.
    ///
    /// `#[serde(default)]` → every pre-feature row deserialises to "nothing
    /// requested", which is also what an operator clearing the form leaves.
    #[serde(default)]
    pub desired_config: DesiredConfig,
    /// What the DEVICE last said it did with [`Self::desired_config`].
    ///
    /// `None` = it has never reported. That is three different situations —
    /// it has not connected since the push, its agent predates
    /// [`RpcCap::ConfigReport`], or nothing has ever been pushed — and the
    /// reader must resolve which from the capability list and the revision,
    /// never assume the last one.
    ///
    /// ⚠️ Read this ALONGSIDE [`Self::desired_config`], never instead of it:
    /// a report whose `revision` is behind the desired one is a device that
    /// has not caught up, which reads on a dashboard exactly like a device
    /// that refused unless the two numbers are compared.
    #[serde(default)]
    pub config_report: Option<ConfigReport>,
    /// FR-40 — the standing request that this device retire its overlay key.
    /// Desired state, exactly like [`Self::desired_config`]: it lives on the
    /// row so a device that is offline rotates on its next connect through
    /// the same reconcile path as one that was online when the admin clicked.
    #[serde(default)]
    pub key_rotation: Option<KeyRotationRequest>,
    /// What the DEVICE last said it did with a rotation order. A claim by the
    /// device, in the [`Self::config_report`] sense — read it against
    /// [`Self::key_rotation`] by `request_id`, never alone.
    #[serde(default)]
    pub key_rotation_report: Option<KeyRotationReport>,
    /// FR-40 — what the device last PRESENTED at its overlay join, stamped by
    /// the server from the join frame it verified. This is the half of a
    /// rotation the control plane can actually vouch for: a report says
    /// "I rotated", the identity says "and here is the key it joined with".
    #[serde(default)]
    pub overlay_identity: Option<OverlayIdentity>,
    /// Subnet-router CIDRs this agent is a gateway for (Phase 2). The SOCKS
    /// mesh longest-prefix-matches a LAN-IP target against these to pick the
    /// covering agent, which then dials the real IP (still gated by the
    /// tenant's `tunnel_policies`). Admin-configured. `#[serde(default)]` →
    /// older rows deserialize to no routes.
    #[serde(default)]
    pub routes: Vec<String>,
    /// Subnet CIDRs the AGENT itself advertises it can route (from its
    /// `advertise_routes` config, refreshed on each `rc:agent.hello`). These
    /// are untrusted SUGGESTIONS — an admin approves a subset into `routes`
    /// (what the mesh actually consumes). `#[serde(default)]` → older rows /
    /// pre-feature agents deserialize to none.
    #[serde(default)]
    pub advertised_routes: Vec<String>,
    /// Multi-region relay PoPs: the agent's nearest relay region (a
    /// `relay_regions` id), derived server-side from its STUN probe reports
    /// with hysteresis. `None` = never probed / all probes timed out — every
    /// issuance path then uses the default region. `#[serde(default)]` →
    /// older rows deserialize to `None`.
    #[serde(default)]
    pub relay_home: Option<String>,
    /// The agent's last full probe table (observability; the UI shows it on
    /// the device detail). `#[serde(default)]` → older rows deserialize to
    /// `None`.
    #[serde(default)]
    pub relay_rtt: Option<Vec<crate::signaling::RelayRegionRtt>>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl Agent {
    pub const COLLECTION: &'static str = "agents";
}

// ────────────────────────────────────────────────────────────────────────────
// FR-51 P2 — ephemeral enrollment keys
// ────────────────────────────────────────────────────────────────────────────

/// A REUSABLE enrollment credential that mints EPHEMERAL devices (FR-51 §4).
///
/// The single-use `enroll-token` is deliberately unusable for autoscaling
/// (ten minutes, once, minted by a human), so this is a second credential
/// kind with an explicitly different risk profile: a standing secret that
/// creates device identities inside an org for as long as it lives. The four
/// controls that make that acceptable are ALL FOUR structural here — a use
/// ceiling (`max_uses`, claimed atomically), an absolute expiry
/// (`expires_at`, enforced on the row as well as in the JWT), revocability
/// (`revoked_at`, checked inside the same atomic claim — expiry alone would
/// mean a leaked key can only be waited out, never stopped), and a per-use
/// audit row ([`EnrollmentKeyUse`]).
///
/// The ephemeral property RIDES THIS CREDENTIAL, never the enrollment
/// request body: a device that could declare itself would either evade the
/// reaper or schedule a permanent device for silent deletion.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnrollmentKey {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// The JWT's `jti`, and the value the use-claim is keyed by. Unique.
    pub jti: String,
    /// Operator label ("ci-runners", "preview-envs"), display only.
    pub label: String,
    /// Admin who minted it — every device it creates records this chain:
    /// key → `created_by`, device → `enroll_key_id`.
    pub created_by: ObjectId,
    /// Use ceiling. `uses` is `$inc`'d inside the same atomic
    /// `find_one_and_update` that checks it, so N racing enrollments can
    /// never mint more than `max_uses` devices between them.
    pub max_uses: i64,
    pub uses: i64,
    pub expires_at: DateTime,
    /// Set = the key is dead, whatever its expiry says. Checked inside the
    /// atomic claim, so revocation takes effect on the very next use.
    pub revoked_at: Option<DateTime>,
    /// Per-device reap TTL this key stamps onto every device it mints
    /// (`Agent::ephemeral_ttl_secs`). `None` = the server default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_ttl_secs: Option<u64>,
    #[serde(default)]
    pub last_used_at: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

impl EnrollmentKey {
    pub const COLLECTION: &'static str = "enrollment_keys";
}

/// One successful use of an [`EnrollmentKey`] — control 4 of FR-51 §4:
/// "which key created this device" stays answerable AFTER the device row is
/// reaped (ephemeral rows hard-delete, so the device row cannot be the
/// audit trail). 90-day TTL like the other audit collections.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnrollmentKeyUse {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub key_id: ObjectId,
    pub agent_id: ObjectId,
    pub machine_id: String,
    pub machine_name: String,
    pub created_at: DateTime,
    /// P4 — when the device this use minted was REMOVED, making the row the
    /// device's whole lifecycle record (born → removed) in one place that
    /// survives both ends: the device row hard-deletes, and `audit_logs` is
    /// a dead collection nothing writes (checked before "the reap in
    /// audit_logs" was implemented as spec'd — recording into a surface
    /// nothing reads would be paperwork, not audit). `None` = still alive,
    /// or removed before P4 shipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_at: Option<DateTime>,
    /// P4 — which path removed it: `ephemeral_expired` (the reaper) /
    /// `ephemeral_self_unenroll` (clean stop) / `agent_delete` (admin).
    /// The same `reason` string every removal path already threads through
    /// `release_overlay_node`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removal: Option<String>,
}

impl EnrollmentKeyUse {
    pub const COLLECTION: &'static str = "enrollment_key_uses";
}

// ────────────────────────────────────────────────────────────────────────────
// Roomler SSH
// ────────────────────────────────────────────────────────────────────────────

/// Bounds on a roomler-SSH session grant. The server clamps before minting;
/// the agent re-clamps on receipt, so a forged or replayed grant cannot ask for
/// a longer-lived session than policy allows.
pub mod ssh_limits {
    /// How long a minted grant stays usable before the target discards it.
    ///
    /// Short on purpose: the grant is consumed by a TCP connection the caller
    /// makes immediately after receiving it, so this only has to cover
    /// signalling latency plus a slow WireGuard handshake. A grant that
    /// lingered would be a standing key to the device.
    pub const GRANT_TTL_SECS: u64 = 60;
    /// Hard ceiling on a session's lifetime, after which the agent closes it.
    /// Twelve hours is long enough for a working day and short enough that a
    /// forgotten terminal does not stay open for a week.
    pub const MAX_SESSION_SECS: u64 = 12 * 3600;
    /// Concurrent SSH sessions one device will serve. Beyond this the agent
    /// refuses rather than queues.
    pub const MAX_CONCURRENT_PER_AGENT: usize = 8;
    /// Grants per minute per (caller, device) pair.
    pub const RATE_LIMIT_PER_MINUTE: u32 = 20;

    /// Clamp a caller-supplied session budget into range, substituting the
    /// ceiling for `None`/0.
    pub fn clamp_session_secs(requested: u64) -> u64 {
        match requested {
            0 => MAX_SESSION_SECS,
            n => n.min(MAX_SESSION_SECS),
        }
    }
}

/// Whether a device accepts roomler-SSH sessions at all.
///
/// Default `Off` — every existing row deserialises to it, so enabling the
/// feature can never retroactively open a device.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SshMode {
    /// Refuse every SSH grant request. The default.
    #[default]
    Off,
    /// Accept grants for callers that clear every other gate.
    On,
}

/// Which local account an SSH session runs as.
///
/// Windows has no password-free way to become an arbitrary local user, so the
/// choices are deliberately the two the daemon can actually reach: its own
/// identity, or the token of whoever is signed in at the console (which the
/// SystemContext machinery already obtains for the lock-screen path). Naming a
/// specific Unix account is the third, and is Unix-only by construction.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SshAccountMode {
    /// The daemon's own identity — SYSTEM on Windows, root under systemd.
    ///
    /// The default *today* because it is what P2 can actually do, and it is
    /// why `SshMode::On` is root-equivalent until the P5 privilege-drop slice
    /// lands. It is not the right long-term default and the admin UI says so.
    #[default]
    Daemon,
    /// The user signed in at the console, via their session token. No password
    /// required, and the session cannot outlive their sign-in.
    ConsoleUser,
    /// A named local account (Unix only; requires the P5 privilege drop).
    Named,
}

/// Device config an operator has requested, for the agent to reconcile
/// (`docs/remote-config.md`).
///
/// Every field is `Option`, and the distinction is load-bearing: `None` means
/// **not managed** — the device keeps whatever it has locally — while `Some`
/// means "this is what it should be". A struct of plain values could not say
/// "leave the rest alone", so an operator toggling exec would silently assert a
/// value for every other key on the surface.
///
/// ⚠️ `remote_config_enabled` is deliberately ABSENT and must never be added.
/// It is the device's opt-in to accepting anything here at all; a server able
/// to set it could opt a device in and then open every other key, which is the
/// one move the whole design exists to prevent.
///
/// ⚠️ `ssh_max_privilege` (M5) is ABSENT for the same reason. It is the
/// device's ceiling on what a SERVER GRANT may run as — the one SSH gate that
/// still holds when the server is the compromised thing — so a server able to
/// set it could raise its own ceiling and the setting would mean nothing. This
/// list is an allowlist, so leaving it out is sufficient; the trap is a later
/// change that adds "the rest of the ssh_* surface" for symmetry.
///
/// ⚠️ **The whole `relay_*` surface (FR-19) is ABSENT**, and unlike the two
/// above this is enforced by a test that matches on the PREFIX rather than on
/// a name. `relay_server_enabled` is gate 4 — the refusal that survives a
/// compromised server — and `relay_server_port` is just as load-bearing: a
/// server able to move the port could point the listener somewhere the
/// operator never opened, or away from one they did. The prefix rule exists
/// because the feature will grow more keys (`relay_max_sessions`,
/// `relay_static_endpoints`), and a per-name test would silently fail to cover
/// the one someone adds later.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct DesiredConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_authorized_keys: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_account_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    /// Bumped on every write. The agent reports the revision it has applied,
    /// so "asked for" and "actually running" can be told apart in the UI —
    /// without it, a device that refused the config and one that never heard
    /// about it look identical.
    #[serde(default)]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime>,
}

impl DesiredConfig {
    /// Whether anything is actually requested. A cleared form is `revision`
    /// plus provenance and no keys — still "nothing to reconcile".
    pub fn is_empty(&self) -> bool {
        self.exec_enabled.is_none()
            && self.ssh_enabled.is_none()
            && self.ssh_authorized_keys.is_none()
            && self.ssh_account_mode.is_none()
            && self.ssh_port.is_none()
    }

    /// Whether this request touches the Fleet-RPC grant. Drives the
    /// `EXEC_DEVICE` requirement — see `remote_config::decide`.
    pub fn touches_exec(&self) -> bool {
        self.exec_enabled.is_some()
    }

    /// Whether this request touches the SSH grant. Every `ssh_*` key counts,
    /// not just `ssh_enabled`: authorized keys decide WHO may connect, and
    /// `ssh_account_mode` decides what a key-list session may run — handing
    /// those out is granting SSH just as much as flipping the switch is.
    pub fn touches_ssh(&self) -> bool {
        self.ssh_enabled.is_some()
            || self.ssh_authorized_keys.is_some()
            || self.ssh_account_mode.is_some()
            || self.ssh_port.is_some()
    }

    /// Does this request ask for the same exec state as `other`?
    ///
    /// Used to tell "granting exec" from "leaving the exec ask exactly as some
    /// other admin already set it" — see `remote_config::decide`.
    pub fn same_exec_as(&self, other: &Self) -> bool {
        self.exec_enabled == other.exec_enabled
    }

    /// [`Self::same_exec_as`] for the SSH family.
    ///
    /// Destructured EXHAUSTIVELY: a new key added to this struct must be
    /// classified as exec, SSH, or neither before this compiles. A key that
    /// silently fell outside both comparisons would let an unchanged-request
    /// carve-out pass a change nobody checked.
    pub fn same_ssh_as(&self, other: &Self) -> bool {
        let Self {
            exec_enabled: _,
            ssh_enabled,
            ssh_authorized_keys,
            ssh_account_mode,
            ssh_port,
            revision: _,
            updated_by: _,
            updated_at: _,
        } = self;
        *ssh_enabled == other.ssh_enabled
            && *ssh_authorized_keys == other.ssh_authorized_keys
            && *ssh_account_mode == other.ssh_account_mode
            && *ssh_port == other.ssh_port
    }
}

/// What a device did with a pushed [`DesiredConfig`].
///
/// ⚠️ Every arm except [`Self::Applied`] / [`Self::Noop`] is a REFUSAL, and the
/// refusals are the reason this type exists. Without them the dashboard cannot
/// tell a device that declined from one that never heard — the two look
/// identical, which is exactly the "a switch that silently does nothing" state
/// the whole feature is trying not to produce.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOutcome {
    /// Reconciled. Which keys are in force NOW and which are merely written
    /// down is in `live` / `needs_restart` — see [`ConfigReport`].
    Applied,
    /// The device already matched; nothing was written. Distinct from
    /// `Applied` with two empty lists so a reader can tell "converged" from
    /// "applied nothing, and I am not sure why".
    Noop,
    /// The device has not opted in: `remote_config_enabled` is off. This is
    /// gate 4 doing its job, not an error — the operator's move is to set the
    /// key ON THE HOST, which no server can do for them.
    NotOptedIn,
    /// The push arrived on a SECONDARY org's socket. `exec_enabled` / `ssh_*`
    /// are machine-wide, so only the primary enrollment may drive them; this
    /// org borrows the device and cannot reconfigure it.
    NotPrimary,
    /// The device tried and failed — see `detail`. Almost always an I/O or
    /// permission problem writing `config.toml`.
    Failed,
}

impl ConfigOutcome {
    /// Did the device actually reconcile? `false` for every refusal, which is
    /// what a caller usually wants to branch on.
    pub fn is_success(self) -> bool {
        matches!(self, Self::Applied | Self::Noop)
    }
}

/// The DEVICE's account of what it did with revision `revision`.
///
/// ⚠️ This is a CLAIM BY THE DEVICE, exactly like [`SshActivityEvent`] and
/// unlike anything in `config_audit`. The audit collection records the
/// SERVER's decision and is authoritative; this records what a host — which
/// may be compromised, may be lying, may simply be old — says happened
/// afterwards. Never fold the two together: a reader has to be able to tell
/// which is which, and only one of them survives a dishonest device.
///
/// ⚠️ An ABSENT report is not evidence of anything. A device that predates
/// [`RpcCap::ConfigReport`] applies the config perfectly well and never says
/// so, which is why the capability is checked before this is read as silence.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ConfigReport {
    /// Which `desired_config.revision` this is about. A report for an older
    /// revision than the row currently holds means the device has not caught
    /// up yet — not that the newer one was refused.
    pub revision: u64,
    pub outcome: ConfigOutcome,
    /// Keys changed and ALREADY in force.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub live: Vec<String>,
    /// Keys written to disk but waiting for a daemon restart.
    ///
    /// ⚠️ Kept SEPARATE from `live` all the way to the screen. Collapsing them
    /// would let the dashboard report SSH as on while the daemon has not
    /// re-spliced the packet path — the device would refuse every session
    /// while the UI insisted it was open.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs_restart: Vec<String>,
    /// Why, for [`ConfigOutcome::Failed`]. Capped — see [`Self::MAX_DETAIL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When the server received it. Server-stamped, never taken off the wire:
    /// a device's clock is not a fact the control plane should inherit.
    pub reported_at: DateTime,
}

impl ConfigReport {
    /// Bound on `detail`, applied on the device AND re-clamped on receipt — a
    /// limit that exists only on the reporting side is not a limit.
    pub const MAX_DETAIL: usize = 400;
}

/// FR-40 — an operator's standing request that a device retire its overlay
/// (WireGuard) key and mint a fresh one (`docs/fr/FR-40-overlay-key-rotation.md`).
///
/// Desired state on the agent row, so the offline case has somewhere to
/// live: the device is ordered on connect if this is present and unanswered.
/// ⚠️ Carries NO key material in either direction — the server orders a
/// re-mint it never sees; the only key that ever crosses the wire is the
/// PUBLIC half in the device's report. A test asserts the order frame has
/// no key-shaped field at all.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct KeyRotationRequest {
    /// Fresh per request. The device echoes it in its report, which is what
    /// lets a report about an EARLIER order be told apart from the answer to
    /// this one — compare ids, never outcomes alone.
    pub request_id: String,
    pub requested_by: ObjectId,
    pub requested_at: DateTime,
    /// When the order was actually pushed to a live socket. Absent = queued
    /// for the device's next connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<DateTime>,
    /// P1c — the overlay PUBLIC key the device held when the order was placed
    /// (its [`OverlayIdentity`] at that moment; absent if it had never
    /// joined). The join under a DIFFERENT key is what proves the rotation:
    /// the device's report rides the session that is about to end and can be
    /// lost (second field run), and a report is only ever a claim anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key_before: Option<String>,
}

/// How a device answered a rotation order. Every arm but [`Self::Rotated`] is
/// a refusal, and — as with [`ConfigOutcome`] — the refusals are the reason
/// the type exists: each has a different fix, and without them a device that
/// declined and one that never heard are the same thing on a screen.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KeyRotationOutcome {
    /// Minted, persisted, and the device is reconnecting under the new key.
    /// The join that follows is what proves it — see [`OverlayIdentity`].
    Rotated,
    /// The device's `overlay_key_rotation` kill switch is off.
    Disabled,
    /// A second order arrived inside the device's own 60 s ceiling.
    RateLimited,
    /// This build has no overlay surface, so it has no key to rotate.
    Unsupported,
    /// Mint or persist failed — see `detail`. The identity is UNCHANGED: a key
    /// that cannot be written down is not adopted, or the next restart would
    /// bring the retired one back.
    Failed,
}

impl KeyRotationOutcome {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Rotated)
    }
}

/// The DEVICE's account of a rotation order — a claim, in the
/// [`ConfigReport`] sense, never the server's own record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct KeyRotationReport {
    /// Echoed from the order.
    pub request_id: String,
    pub outcome: KeyRotationOutcome,
    /// Base64 WireGuard PUBLIC keys. Public by construction — the device
    /// never has a reason to send the secret half, and the server never
    /// stores one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_public_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_public_key: Option<String>,
    /// The epoch the device will present on its next join (bumped per
    /// rotation, persisted next to the key).
    #[serde(default)]
    pub key_epoch: u32,
    /// Why, for the refusals. Redacted and capped by the device; re-clamped
    /// on receipt (see [`Self::MAX_DETAIL`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Server-stamped, never taken off the wire.
    pub reported_at: DateTime,
}

impl KeyRotationReport {
    pub const MAX_DETAIL: usize = 400;
}

/// What a device last presented at its overlay join, as the server verified
/// it (`ws::overlay`). Stamped on every join, so it is also simply "this
/// device's current overlay public key" for the dashboard.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct OverlayIdentity {
    /// Base64 WireGuard public key.
    pub public_key: String,
    #[serde(default)]
    pub key_epoch: u32,
    pub joined_at: DateTime,
}

/// Per-device roomler-SSH policy — gate 3 of four, the exact shape of
/// [`ExecPolicy`] because the reasoning is identical and a reader who knows one
/// should recognise the other.
///
/// Deliberately NOT folded into [`ExecPolicy`]: "may run one clamped command"
/// and "may hold an interactive session" are different grants, and an operator
/// who enabled the first must not silently acquire the second.
///
/// ⚠️ With [`SshAccountMode::Daemon`] — the current default — `SshMode::On` is
/// root-equivalent by construction, exactly like `ExecMode::On`.
/// `PartialEq` for the same reason [`ExecPolicy`] carries it, and it matters
/// more here: the untouched default names [`SshAccountMode::Daemon`], so an
/// API that could not tell "never configured" from "explicitly all-defaults"
/// would hand the dialog a pre-selected root shell.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct SshPolicy {
    /// Does this device accept SSH sessions? Gate 3.
    #[serde(default)]
    pub mode: SshMode,
    /// May this device *originate* SSH against other devices (the
    /// `roomler ssh` CLI path, where the daemon's own agent WS carries the
    /// request)? Default `false` — without it, compromising any enrolled
    /// laptop would inherit its owner's SSH rights across the whole fleet.
    #[serde(default)]
    pub can_originate: bool,
    /// Restrict callers to these users. Empty = no user restriction (the
    /// permission bit + the other gates still apply).
    #[serde(default)]
    pub allowed_user_ids: Vec<ObjectId>,
    /// Restrict callers to holders of these roles. Empty = no role
    /// restriction.
    #[serde(default)]
    pub allowed_role_ids: Vec<ObjectId>,
    /// Which local account sessions run as.
    #[serde(default)]
    pub account_mode: SshAccountMode,
    /// The account for [`SshAccountMode::Named`]. Ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// How consent is obtained before a session opens. `None` = the safe
    /// default ([`ConsentMode::Prompt`]); unattended servers set `Auto`.
    /// Only `Auto` and `Prompt` are honoured, as for exec.
    #[serde(default)]
    pub consent_mode: Option<ConsentMode>,
}

impl SshPolicy {
    /// Split into the two halves the server actually uses: what is checked at
    /// AUTHORIZE time ([`SshGates`]) and what crosses to the device in the
    /// grant at MINT time ([`SshGrantSpec`]).
    ///
    /// The destructure is deliberately EXHAUSTIVE — no `..` — and this is the
    /// only sanctioned way to consume a policy for a session decision. A field
    /// added to `SshPolicy` therefore stops compiling HERE, and stays broken
    /// until someone routes it into one of the halves in writing. That is the
    /// point: `consent_mode` crossed the wire for weeks while being
    /// destructured away (`consent_mode: _`), and reading fields off the
    /// policy à la carte is exactly how the next field gets the same
    /// treatment.
    pub fn split(self) -> (SshGates, SshGrantSpec) {
        let SshPolicy {
            mode,
            can_originate,
            allowed_user_ids,
            allowed_role_ids,
            account_mode,
            account,
            consent_mode,
        } = self;
        (
            SshGates {
                mode,
                can_originate,
                allowed_user_ids,
                allowed_role_ids,
            },
            SshGrantSpec {
                account_mode,
                account,
                consent_mode,
            },
        )
    }
}

/// The authorize-time half of an [`SshPolicy`] — everything consulted before
/// the server decides a grant may exist at all.
#[derive(Debug, Clone)]
pub struct SshGates {
    /// Does this device accept SSH sessions? Gate 3.
    pub mode: SshMode,
    /// May this device ORIGINATE sessions (`roomler ssh` over its own WS)?
    /// Not a gate for inbound sessions — it is read off the ORIGIN device's
    /// policy on the device-originated leg.
    pub can_originate: bool,
    pub allowed_user_ids: Vec<ObjectId>,
    pub allowed_role_ids: Vec<ObjectId>,
}

impl SshGates {
    /// Is `user` allowed by the user/role allowlists?
    ///
    /// Empty lists mean "no restriction at this layer" — never "deny all" —
    /// because the permission bit and the org kill-switch are the layers that
    /// decide *whether* anyone may connect. Mirrors the exec check so the two
    /// policies cannot drift apart in meaning.
    pub fn allows_caller(&self, user_id: &ObjectId, role_ids: &[ObjectId]) -> bool {
        if !self.allowed_user_ids.is_empty() && !self.allowed_user_ids.contains(user_id) {
            return false;
        }
        if !self.allowed_role_ids.is_empty()
            && !role_ids.iter().any(|r| self.allowed_role_ids.contains(r))
        {
            return false;
        }
        true
    }
}

/// The mint-time half of an [`SshPolicy`] — what the grant carries to the
/// device. Its fields map 1:1 onto `ServerMsg::SshGrant`, and the mint site
/// destructures it exhaustively, so a field routed here cannot be dropped on
/// the way to the wire.
#[derive(Debug, Clone)]
pub struct SshGrantSpec {
    pub account_mode: SshAccountMode,
    pub account: Option<String>,
    /// Raw policy value. Only an explicit `Auto` may cross as `Auto`; the mint
    /// site collapses everything else — including unset — to `Prompt`, the
    /// attended default (mirroring [`ExecPolicy::effective_consent_mode`]).
    pub consent_mode: Option<ConsentMode>,
}

// ────────────────────────────────────────────────────────────────────────────
// Tunnel client (roomler-cli)
// ────────────────────────────────────────────────────────────────────────────

/// A laptop running `roomler`. Mirrors [`Agent`] structurally
/// (same lifecycle, same `AgentStatus`, same `(tenant_id, machine_id)`
/// uniqueness for rehydrate-on-re-enroll) but slimmer — tunnel clients
/// don't capture screens or hold capability lists. The `_role_` is
/// inverted vs an agent: a tunnel client *initiates* forwards; an
/// agent *serves* them.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TunnelClient {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// User who installed + runs the CLI on this laptop. Carried into
    /// `TunnelClientClaims.owner_user_id` at enrollment time and
    /// recorded in every `tunnel_audit` row.
    pub owner_user_id: ObjectId,
    pub name: String,
    /// Friendly label an admin sets purely for display (see `Agent::display_name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Free-form admin labels for fleet filtering/search.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// True once an admin renamed this client server-side (see
    /// `Agent::name_admin_set` — same rehydrate-clobber protection; a
    /// CLIENT-side rename can't rename in place at all, it derives a new
    /// machine_id and enrolls a new row).
    #[serde(default)]
    pub name_admin_set: bool,
    pub machine_id: String,
    pub os: OsKind,
    pub client_version: String,
    pub status: AgentStatus,
    pub last_seen_at: DateTime,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl TunnelClient {
    pub const COLLECTION: &'static str = "tunnel_clients";
}

// ────────────────────────────────────────────────────────────────────────────
// Tunnel policy
// ────────────────────────────────────────────────────────────────────────────
//
// Single source of truth for ACL data shapes — DB rows AND the
// evaluator in `tunnel-core::policy` both consume these types. The
// evaluator re-exports them so callers have one import path; this
// keeps the DB schema authoritative without inverting the dep graph
// (`services` already depends on `remote_control` for `Agent` etc.).

/// Matches a destination hostname. Adjacently tagged so JSON wire
/// shape is `{"kind":"exact","value":"db.intranet"}` etc.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HostPattern {
    /// Literal match — `"db.intranet"`.
    Exact(String),
    /// Glob — `"*.intranet"` matches one or more subdomains.
    Wildcard(String),
    /// CIDR range — `"10.0.0.0/24"`. Resolves against literal IPs only;
    /// hostnames must be resolved by the caller first.
    Cidr(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PortRange {
    /// Inclusive lower bound.
    pub low: u16,
    /// Inclusive upper bound. Equal to `low` for single-port rules.
    pub high: u16,
}

/// Which L4 protocol a [`DestinationRule`] permits. `Any` (the default,
/// and what pre-UDP stored rules deserialise to) matches both TCP
/// CONNECT forwards and UDP ASSOCIATE forwards; `Tcp` / `Udp` narrow a
/// rule to one. The forward gate evaluates the request's protocol
/// against this via [`ProtocolKind::permits`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolKind {
    Tcp,
    Udp,
    /// Matches any protocol. Default so a rule authored (or stored)
    /// without a `proto` field keeps its pre-UDP behaviour.
    #[default]
    Any,
}

impl ProtocolKind {
    /// Does a rule declaring `self` permit a forward request of
    /// protocol `req`? `Any` permits everything; otherwise the request
    /// must match exactly. `req` is always concrete (`Tcp` / `Udp`) —
    /// a request never carries `Any`.
    pub fn permits(self, req: ProtocolKind) -> bool {
        matches!(self, ProtocolKind::Any) || self == req
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DestinationRule {
    pub host_pattern: HostPattern,
    pub port_range: PortRange,
    /// L4 protocol this rule permits. `#[serde(default)]` → `Any` for
    /// pre-UDP stored rules + omitting it on the wire. Gated in
    /// `tunnel_core::policy::evaluate`.
    #[serde(default)]
    pub proto: ProtocolKind,
}

/// Match a `(dst_host, dst_port)` tuple against a single destination
/// rule.
///
/// Lived in `tunnel_core::policy` until P3e lever E moved it HERE, next to
/// the shapes it matches over (this crate is where `HostPattern` /
/// `PortRange` / `DestinationRule` are canonical — the doc block above this
/// section says so). The move lets `roomler-node-core`'s config-side ACL
/// evaluate rules without depending on tunnel-core's data plane; tunnel-core
/// re-exports both fns from `policy` so its callers are unchanged.
pub fn dst_matches(rule: &DestinationRule, dst_host: &str, dst_port: u16) -> bool {
    if dst_port < rule.port_range.low || dst_port > rule.port_range.high {
        return false;
    }
    host_matches(&rule.host_pattern, dst_host)
}

pub fn host_matches(pattern: &HostPattern, host: &str) -> bool {
    match pattern {
        HostPattern::Exact(s) => s.eq_ignore_ascii_case(host),
        HostPattern::Wildcard(s) => match s.strip_prefix("*.") {
            Some(suffix) => {
                host.to_ascii_lowercase()
                    .ends_with(&suffix.to_ascii_lowercase())
                    && host.len() > suffix.len()
                    && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
            }
            // A wildcard without a leading "*." is treated as exact —
            // safer than allow-all.
            None => s.eq_ignore_ascii_case(host),
        },
        HostPattern::Cidr(cidr) => match (
            cidr.parse::<ipnet::IpNet>(),
            host.parse::<std::net::IpAddr>(),
        ) {
            (Ok(net), Ok(ip)) => net.contains(&ip),
            _ => false,
        },
    }
}

/// Who a policy applies to. `{"kind":"all_users"}` is the catch-all
/// (default-allow lite — still scoped to the tenant). Externally
/// tagged would be cleaner but mixes object-vs-string on the wire
/// when one variant is a unit; `tag = "kind"` keeps everything as
/// objects.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicySubject {
    UserId {
        #[serde(rename = "id")]
        user_id: ObjectId,
    },
    RoleId {
        #[serde(rename = "id")]
        role_id: ObjectId,
    },
    TunnelClientId {
        #[serde(rename = "id")]
        tunnel_client_id: ObjectId,
    },
    /// A specific agent acting as a tunnel CLIENT (node-stack unification,
    /// P3b-2). Orthogonal to `PolicyTarget::AgentId` (which names the forward's
    /// DESTINATION): here the agent is the ORIGIN of the tunnel. Purely additive
    /// — old policy docs never carry this variant, so no migration is needed.
    AgentId {
        #[serde(rename = "id")]
        agent_id: ObjectId,
    },
    /// Every user in the policy's tenant.
    AllUsers,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyTarget {
    AgentId {
        #[serde(rename = "id")]
        agent_id: ObjectId,
    },
    /// Every agent in the policy's tenant.
    AllAgents,
}

/// A tenant-scoped allowlist. Default-deny: a forward is permitted
/// only if at least one matching policy exists. See plan §"Security
/// model" + `tunnel-core::policy::evaluate` for the eval semantics.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TunnelPolicy {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub name: String,
    pub subjects: Vec<PolicySubject>,
    pub targets: Vec<PolicyTarget>,
    pub allowlist: Vec<DestinationRule>,
    /// Per-session concurrent-flow ceiling. `None` = unlimited.
    /// Default 64 in v1 (covers JDBC pools comfortably).
    pub max_concurrent_flows: Option<u32>,
    /// Per-session byte ceiling (sum of bytes_in + bytes_out).
    /// `None` = unlimited.
    pub max_bytes_per_session: Option<u64>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl TunnelPolicy {
    pub const COLLECTION: &'static str = "tunnel_policies";
}

// ────────────────────────────────────────────────────────────────────────────
// Overlay ACL
// ────────────────────────────────────────────────────────────────────────────

/// Who an [`OverlayPolicy`] applies to — the node ORIGINATING traffic.
///
/// Deliberately a separate enum from [`PolicySubject`]: the overlay is pure
/// L3, so the tunnel's client/agent-origin distinction is meaningless here,
/// and a node is addressed by its `overlay_nodes` id regardless of whether it
/// is backed by an agent or a tunnel client. Reusing `PolicySubject` would
/// offer variants that can never match an overlay node.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlaySelector {
    /// One specific overlay node.
    NodeId {
        #[serde(rename = "id")]
        node_id: ObjectId,
    },
    /// Every node owned by this user (resolved through the node's backing
    /// agent / tunnel-client `owner_user_id`).
    UserId {
        #[serde(rename = "id")]
        user_id: ObjectId,
    },
    /// Every node whose owner holds this role.
    RoleId {
        #[serde(rename = "id")]
        role_id: ObjectId,
    },
    /// Every node in the policy's tenant.
    AllNodes,
}

/// Which node a policy lets the sources reach — the peer, and (when it is a
/// subnet router) the gateway for its approved CIDRs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayTarget {
    NodeId {
        #[serde(rename = "id")]
        node_id: ObjectId,
    },
    /// Every node in the policy's tenant.
    AllNodes,
}

/// One destination grant: a CIDR plus an optional port/protocol narrowing.
///
/// ⚠️ `port_range` / `proto` are **stored and distributed but do not affect
/// netmap shaping**, because peer visibility and route lists are the only
/// primitives the netmap wire format can express — there is no port dimension
/// in [`crate::signaling::NetmapPeer`]. They take effect only once the
/// node-side ingress filter consumes them. Until then a port-narrowed rule
/// grants the whole peer at L3, so the admin UI must say so rather than imply
/// a narrowing that is not enforced.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OverlayRule {
    /// Destination prefix, e.g. `"10.84.6.0/24"` or a peer's `"100.64.0.7/32"`.
    pub cidr: String,
    #[serde(default = "OverlayRule::all_ports")]
    pub port_range: PortRange,
    #[serde(default)]
    pub proto: ProtocolKind,
}

impl OverlayRule {
    fn all_ports() -> PortRange {
        PortRange {
            low: 1,
            high: u16::MAX,
        }
    }
}

/// How strictly a tenant's overlay ACL is applied. Stored on the tenant's
/// [`OverlayNetwork`] (one row per tenant) rather than on each policy — it is
/// a network-wide posture, and putting it there means the join path already
/// has it loaded.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OverlayAclMode {
    /// Legacy behaviour: every node sees every peer and every approved route.
    /// The default, so enabling the feature never breaks a live mesh.
    #[default]
    Off,
    /// Evaluate and log what WOULD be denied, but ship the permissive netmap.
    /// The safe way to author policies against real traffic before cutting over.
    Warn,
    /// Evaluate and enforce.
    Enforce,
}

/// FR-19 gate 1 — the org's switch for peer relays. Same shape and the same
/// closed default as [`OverlayAclMode`], for the same reason: every
/// `overlay_networks` row written before FR-19 lacks the field, and the
/// default must make enabling the feature a no-op for them.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum PeerRelayMode {
    /// No sessions are minted. The default.
    #[default]
    Off,
    /// Decide and audit what WOULD be minted, but mint nothing.
    Warn,
    /// Mint.
    On,
}

/// FR-19 gate 3 — may THIS device serve as an org relay for its tenant?
/// Per-device and admin-set; the device's own `relay_server_enabled` (gate 4)
/// is separate and device-local, so approving here does nothing on a device
/// that has not opted in itself — an offer is not a grant, and a grant is not
/// an offer.
///
/// `PartialEq` so a listing can ask "is this the untouched default?" — the
/// same question [`ExecPolicy`] answers for the same reason.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct PeerRelayPolicy {
    /// Approved to serve. Default `false`.
    #[serde(default)]
    pub serve: bool,
    /// Admin-declared `ip:port`s the relay is reachable at — the port-forwarded
    /// or multi-homed case the node's own srflx/static candidates cannot
    /// discover. Each entry is SSRF-validated at approval time (a public IP
    /// literal, never a name) and re-checked at mint time: a server-pushed
    /// probe target is an oracle-returning port scanner run by every device
    /// in the tenant as SYSTEM/root, so `169.254.169.254:80` must never get
    /// through here. Tried AFTER the measured candidates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub static_endpoints: Vec<String>,
}

/// Bounds on FR-19 relay minting. Server-side only — the relay device
/// re-clamps every session lifetime against its own table
/// (`orgrelay::session`), so nothing here is trusted by a device.
pub mod peer_relay_limits {
    /// Mints per minute per (requesting node, relay node) pair — the key the
    /// spec prescribes (§4 "Rate limiting"): a mint writes state onto a THIRD
    /// device, so the pair to bound is requester × relay, not requester × peer.
    /// The number is the exec precedent (`exec_limits::RATE_LIMIT_PER_MINUTE`)
    /// so a fleet boot — one requester asking for every unreachable peer
    /// through the same relay inside a minute — fits under it.
    pub const MINT_RATE_LIMIT_PER_MINUTE: u32 = 30;
    /// The relay-server UDP port assumed when a join carries none — the
    /// value E2E-3 settled (spec §5) and `orgrelay::DEFAULT_RELAY_SERVER_PORT`
    /// on the device; the two are the same number by contract.
    pub const DEFAULT_RELAY_PORT: u16 = 3478;
    /// Seconds a member has to complete its bind at the relay.
    pub const BIND_SECS: u32 = 30;
    /// Idle seconds before the relay drops a bound session. Refreshed by
    /// traffic, so under a WireGuard keepalive it never fires — which is why
    /// `MAX_LIFETIME_SECS` exists.
    pub const IDLE_SECS: u32 = 300;
    /// Absolute session lifetime, independent of traffic. Also the exposure
    /// window after a server restart loses the session registry: a session
    /// the server can no longer revoke ends here at the latest.
    pub const MAX_LIFETIME_SECS: u32 = 3600;
    /// Live sessions the server will place on one relay — mirrors the
    /// device's own `orgrelay::session::MAX_SESSIONS`, which refuses beyond it.
    pub const MAX_SESSIONS_PER_RELAY: usize = 64;
    /// How long a reachability report counts toward relay ranking.
    pub const PROBE_TTL_SECS: u64 = 600;
    /// The STUN magic cookie as a 24-bit VNI — never minted, so a STUN
    /// packet arriving at the relay port can never alias a session.
    pub const STUN_COOKIE_VNI: u32 = 0x2112A4;
    /// The 24-bit Geneve VNI ceiling.
    pub const VNI_MAX: u32 = 0xFF_FFFF;
}

/// Why a peer-relay decision — an admin's approval or a session mint — was
/// refused. The FR-19 twin of [`ExecDenyReason`] / [`SshDenyReason`], kept
/// separate for the reason those two are: the vocabularies diverge (a mint has
/// no caller permission, an approval has no relay).
///
/// Every arm is auditable. A refusal nobody can query is a refusal nobody will
/// notice, and the refused rows are what an operator hunting "why is this pair
/// still on DERP?" actually reads.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerRelayDenyReason {
    /// Gate 1 — the org's [`PeerRelayMode`] is `Off`. ⚠️ Deliberately NEVER
    /// audited: with the feature off there must be zero rows and zero reads
    /// (acceptance: "`peer_relay_mode=off` ⇒ no `peer_relay_audit` rows").
    OrgDisabled,
    /// Approval — the caller lacks `MANAGE_AGENTS`.
    NotDeviceAdmin,
    /// Approval — turning `serve` ON additionally requires `EXEC_DEVICE`: you
    /// cannot grant a power you do not hold (#600/#605), and nominating a
    /// relay makes a device a chokepoint for the whole tenant's traffic.
    CannotGrantRelay,
    /// The requesting node did not advertise `supports_org_relay` on its join
    /// — a pre-FR-19 build that could not act on a session anyway.
    RequesterUnsupported,
    /// The peer the requester wants to reach did not advertise it; a session
    /// with one deaf end is a session nobody can use.
    PeerUnsupported,
    /// The peer is in another tenant.
    CrossTenant,
    /// Gate 2 — no overlay policy grants this pair relay use. Evaluated
    /// regardless of `acl_mode`: an affirmative capability (spec §4).
    AclDenied,
    /// Gate 2 could not be evaluated because the policy rows were unreadable.
    /// The fail-CLOSED arm, distinct from [`Self::AclDenied`] so a Mongo blip
    /// is never mistaken for a policy decision.
    PolicyUnreadable,
    /// No device in the tenant is approved (gate 3), advertising
    /// `relay-server` (gate 4) and online.
    NoRelay,
    /// The chosen relay's endpoint failed the SSRF validator — it would have
    /// steered members at a loopback / RFC1918 / metadata / overlay address.
    NonRoutableEndpoint,
    /// The requester or the relay is a SECONDARY-org row on a multi-org
    /// device; serving is primary-only (a UDP listener is host-global).
    SecondaryOrg,
    /// Per-(requesting node, relay node) ceiling
    /// ([`peer_relay_limits::MINT_RATE_LIMIT_PER_MINUTE`]).
    RateLimited,
}

impl PeerRelayDenyReason {
    /// One line an operator can act on — names WHICH gate said no.
    pub fn message(self) -> &'static str {
        match self {
            Self::OrgDisabled => "peer relays are off for this organization",
            Self::NotDeviceAdmin => "Missing MANAGE_AGENTS permission",
            Self::CannotGrantRelay => {
                "Approving a device as an org relay requires EXEC_DEVICE — it makes the device \
                 a traffic chokepoint for the whole organization, and you cannot grant a power \
                 you do not hold"
            }
            Self::RequesterUnsupported => "the requesting device does not support org relays",
            Self::PeerUnsupported => "the peer does not support org relays",
            Self::CrossTenant => "the peer is not in this organization",
            Self::AclDenied => "no overlay policy grants this pair the use of a relay",
            Self::PolicyUnreadable => {
                "the overlay policies could not be read; refusing rather than guessing"
            }
            Self::NoRelay => "no approved, serving, online relay device in this organization",
            Self::NonRoutableEndpoint => "the relay's endpoint is not a public address",
            Self::SecondaryOrg => "org relays serve the device's primary organization only",
            Self::RateLimited => "too many relay requests through this relay; try again shortly",
        }
    }
}

/// Which decision a [`PeerRelayAuditEvent`] records.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerRelayAuditAction {
    /// An admin set a device's [`PeerRelayPolicy`] — the privilege-granting
    /// act, audited because the exit-node precedent auditing nothing is a gap
    /// not to copy.
    Approve,
    /// A node asked for a session to a peer and the server decided.
    Mint,
    /// The server tore a session down — org mode off, an ACL edit that no
    /// longer grants the pair the relay, the relay's approval cleared, or a
    /// party removed. A push, never an expiry (§7): the idle deadline never
    /// fires under a WireGuard keepalive.
    Revoke,
}

/// One peer-relay decision, granted or refused. TTL-expired after 90 days like
/// [`ExecAuditEvent`].
///
/// One collection for both actions on purpose: the question an incident review
/// asks — "who made this device a relay, and what has been routed through it
/// since?" — is ONE query on `agent_id`, which two collections would split.
/// The optional fields each belong to one action; [`Self::action`] says which.
///
/// **A mint row records the DECISION, not the session.** The server pushes a
/// session to three devices and its involvement ends there; it never sees a
/// byte of what they exchange. Same discipline as [`SshAuditEvent`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PeerRelayAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub action: PeerRelayAuditAction,
    /// The device the row is ABOUT: the one being approved, or the relay
    /// chosen to carry the session (absent on a mint no relay qualified for).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<ObjectId>,
    /// The admin who asked (`approve`). A mint has no person behind it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<ObjectId>,
    /// `mint` — the node that asked, and the peer it asked to reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_node_id: Option<ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_node_id: Option<ObjectId>,
    /// `mint` — the overlay node chosen as the relay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_node_id: Option<ObjectId>,
    /// `approve` — the requested value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serve: Option<bool>,
    /// `mint` — the VNI issued. Absent on refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vni: Option<u32>,
    /// `mint` under [`PeerRelayMode::Warn`]: the decision was taken and
    /// recorded exactly as `On` would have, and NOTHING was pushed.
    #[serde(default)]
    pub warn_only: bool,
    pub at: DateTime,
    /// Refusal reason; `None` = approved / minted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<PeerRelayDenyReason>,
    /// `revoke` — which of the four triggers fired (`mode_off` |
    /// `acl_revoked` | `policy_revoked` | `device_removed` | `device_left`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PeerRelayAuditEvent {
    pub const COLLECTION: &'static str = "peer_relay_audit";
}

#[cfg(test)]
mod peer_relay_audit_tests {
    use super::*;

    /// The wire strings are what the audit UI and the integration tests match
    /// on; a rename here would silently turn a filtered view empty.
    #[test]
    fn deny_reason_wire_strings_are_locked() {
        let all = [
            (PeerRelayDenyReason::OrgDisabled, "org_disabled"),
            (PeerRelayDenyReason::NotDeviceAdmin, "not_device_admin"),
            (PeerRelayDenyReason::CannotGrantRelay, "cannot_grant_relay"),
            (
                PeerRelayDenyReason::RequesterUnsupported,
                "requester_unsupported",
            ),
            (PeerRelayDenyReason::PeerUnsupported, "peer_unsupported"),
            (PeerRelayDenyReason::CrossTenant, "cross_tenant"),
            (PeerRelayDenyReason::AclDenied, "acl_denied"),
            (PeerRelayDenyReason::PolicyUnreadable, "policy_unreadable"),
            (PeerRelayDenyReason::NoRelay, "no_relay"),
            (
                PeerRelayDenyReason::NonRoutableEndpoint,
                "non_routable_endpoint",
            ),
            (PeerRelayDenyReason::SecondaryOrg, "secondary_org"),
            (PeerRelayDenyReason::RateLimited, "rate_limited"),
        ];
        for (reason, wire) in all {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::json!(wire)
            );
            assert!(!reason.message().is_empty());
        }
        assert_eq!(
            serde_json::to_value(PeerRelayAuditAction::Approve).unwrap(),
            serde_json::json!("approve")
        );
        assert_eq!(
            serde_json::to_value(PeerRelayAuditAction::Mint).unwrap(),
            serde_json::json!("mint")
        );
        assert_eq!(
            serde_json::to_value(PeerRelayAuditAction::Revoke).unwrap(),
            serde_json::json!("revoke")
        );
    }

    /// A row must explain itself without a join, and the optional fields are
    /// skipped when absent so an `approve` row carries no mint noise.
    #[test]
    fn approve_row_serialises_without_mint_fields() {
        let row = PeerRelayAuditEvent {
            id: None,
            tenant_id: ObjectId::new(),
            action: PeerRelayAuditAction::Approve,
            agent_id: Some(ObjectId::new()),
            user_id: Some(ObjectId::new()),
            requester_node_id: None,
            peer_node_id: None,
            relay_node_id: None,
            serve: Some(true),
            vni: None,
            warn_only: false,
            at: DateTime::now(),
            denied: Some(PeerRelayDenyReason::CannotGrantRelay),
            reason: None,
        };
        let doc = bson::to_document(&row).unwrap();
        assert!(doc.contains_key("serve"));
        assert!(!doc.contains_key("vni"));
        assert!(!doc.contains_key("requester_node_id"));
        assert_eq!(doc.get_str("denied").unwrap(), "cannot_grant_relay");
        let back: PeerRelayAuditEvent = bson::from_document(doc).unwrap();
        assert_eq!(back.action, PeerRelayAuditAction::Approve);
        assert_eq!(back.denied, Some(PeerRelayDenyReason::CannotGrantRelay));
    }
}

/// A tenant-scoped overlay access rule. Default-deny **once the tenant's
/// [`OverlayAclMode`] is `Enforce`**; `Off` (the default) preserves the
/// historical "same tenant + same network ⇒ full visibility" behaviour.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OverlayPolicy {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub name: String,
    /// Lets an admin park a rule without deleting it.
    #[serde(default = "crate::models::default_true")]
    pub enabled: bool,
    pub sources: Vec<OverlaySelector>,
    pub via: Vec<OverlayTarget>,
    pub destinations: Vec<OverlayRule>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl OverlayPolicy {
    pub const COLLECTION: &'static str = "overlay_policies";
}

pub(crate) fn default_true() -> bool {
    true
}

// ────────────────────────────────────────────────────────────────────────────
// Tunnel audit
// ────────────────────────────────────────────────────────────────────────────

/// What happened. Drives the audit-log roll-up + the admin search
/// view in T4. Wire form is snake_case for consistency with every
/// other enum in this module. Distinct from the existing
/// `AuditKind` (remote-control sessions) — different collection,
/// different concerns, different consumers.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TunnelAuditKind {
    /// WebRTC peer was opened (one per `tunnel forward` invocation).
    PeerOpen,
    /// WebRTC peer was torn down (Ctrl-C, idle-timeout, etc.).
    PeerClose,
    /// `TcpForwardRequest` was allowed and the agent dialed dst
    /// successfully. Has flow_id + dst_host + dst_port set.
    TcpAccept,
    /// `TcpForwardRequest` was denied — by the server-side ACL gate
    /// OR by the agent's belt-and-suspenders allowlist. Reason +
    /// `RejectKind` carried in the `reason` field.
    TcpReject,
    /// Agent tried to dial dst, got a hard failure (timeout / refused
    /// / dns). Separate from `TcpReject` so the dashboard can
    /// distinguish "policy denied" from "network broken".
    TcpDialFailed,
    /// Flow closed cleanly or via I/O error.
    TcpClosed,
    /// Per-policy concurrency or byte ceiling hit.
    RateLimited,
    /// WS revocation re-check fired mid-session (admin set
    /// `Quarantined` or soft-deleted the row).
    StatusRevoke,
}

/// Which relay path the peer connection ended up using. Direct =
/// UDP hole punch worked; TurnUdp/Tcp = went through coturn (counts
/// against our bandwidth bill). Set on `PeerOpen` once ICE finishes
/// gathering, repeated on `PeerClose` for easy aggregation.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RelayMode {
    Direct,
    TurnUdp,
    TurnTcp,
}

/// Append-only audit event. One row per interesting happening, keyed
/// by `tunnel_session_id` so a single session reconstruct is
/// `find({tunnel_session_id: …}).sort({at: 1})`. 90 d TTL — see
/// `crates/db/src/indexes.rs::tunnel_audit`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TunnelAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// Correlation key — every event for one peer lifetime shares
    /// this id. New ObjectId per `tunnel forward` invocation.
    pub tunnel_session_id: ObjectId,
    /// The originating tunnel-CLIENT row, set when a dedicated
    /// `roomler` client opened this session. `None` for an
    /// agent-originated session (P3b-2), where `origin_agent_id` is set
    /// instead. Exactly one of `tunnel_client_id` / `origin_agent_id`
    /// is populated. Optional (not a bare `ObjectId`) since P3b-2 —
    /// old rows carry a bare id which still deserialises into `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_client_id: Option<ObjectId>,
    /// The originating AGENT, set when an enrolled agent drove the
    /// tunnel-client role over its own WS (P3b-2). `None` for a
    /// dedicated-client session. `agent_id` below is always the TARGET
    /// of the tunnel, regardless of origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_agent_id: Option<ObjectId>,
    pub agent_id: ObjectId,
    pub user_id: ObjectId,
    pub at: DateTime,
    pub kind: TunnelAuditKind,
    /// Set for per-flow events (TcpAccept / TcpReject /
    /// TcpDialFailed / TcpClosed / RateLimited); None for
    /// peer-lifetime events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dst_port: Option<u16>,
    #[serde(default)]
    pub bytes_in: u64,
    #[serde(default)]
    pub bytes_out: u64,
    /// Inferred proxy for "amount of activity" — packet count proxy
    /// (DC messages received). Helps distinguish bulk transfer from
    /// interactive sessions in the dashboard.
    #[serde(default)]
    pub message_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    pub relay: RelayMode,
    /// Source IP of the tunnel client's WS connection (from
    /// X-Forwarded-For on the WS upgrade). Forensic baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_src_ip: Option<String>,
    /// Source port on the agent's outgoing TCP socket — lets the DB's
    /// own audit log be correlated with this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_src_port: Option<u16>,
    pub client_version: String,
    pub client_os: OsKind,
    /// Free-form reason field (e.g. `"acl_denied: no matching policy"`,
    /// `"dial: connection refused"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TunnelAuditEvent {
    pub const COLLECTION: &'static str = "tunnel_audit";
}

// ────────────────────────────────────────────────────────────────────────────
// Fleet-RPC audit
// ────────────────────────────────────────────────────────────────────────────

/// The result of one remote command — the Hub's currency, the HTTP response
/// body, and the payload of the cross-pod `exec.dispatch` reply. Mirrors
/// `ClientMsg::RpcResult` minus the correlation id.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecOutcome {
    /// `None` when the command never ran or was killed — see
    /// [`Self::error`]. A caller must be able to tell that apart from
    /// "ran and exited 0".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Already redacted + capped by the agent.
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Why an exec request was refused. `None` on the audit row means it ran.
///
/// Every denial is recorded — a refused exec is the interesting one, and
/// without it a probing caller leaves no trace.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecDenyReason {
    /// Gate 1 — the org's `remote_exec_enabled` kill-switch is off.
    OrgDisabled,
    /// Gate 2 — the caller lacks `EXEC_DEVICE`.
    NoPermission,
    /// Gate 3 — the target device's [`ExecPolicy::mode`] is `Off`.
    DeviceDisabled,
    /// Gate 3 — the caller isn't in the device's allowed users/roles.
    CallerNotAllowed,
    /// Gate 3 — the requested shell isn't in the device's allowed list.
    ShellNotAllowed,
    /// The CLI origin device isn't blessed with `can_originate`.
    OriginNotAllowed,
    /// The agent doesn't advertise the `exec` RPC capability.
    Unsupported,
    /// The device is offline.
    Offline,
    /// The operator denied the consent prompt (or it timed out).
    ConsentDenied,
    /// Per-(user, device) rate limit.
    RateLimited,
    /// Gate 4 — the agent's own `exec_enabled` config key is false. Reported
    /// by the agent rather than decided server-side.
    AgentDisabled,
}

/// Why a roomler-SSH grant was refused. The twin of [`ExecDenyReason`]; kept
/// separate so the two features' reasons cannot drift into each other's
/// vocabulary as they diverge (SSH grows account modes and session limits that
/// mean nothing to exec).
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SshDenyReason {
    /// Gate 1 — the org's `remote_ssh_enabled` kill-switch is off.
    OrgDisabled,
    /// Gate 2 — the caller lacks `SSH_DEVICE`.
    NoPermission,
    /// Gate 3 — the target device's [`SshPolicy::mode`] is `Off`.
    DeviceDisabled,
    /// Gate 3 — the caller isn't in the device's allowed users/roles.
    CallerNotAllowed,
    /// The originating device isn't blessed with `can_originate`.
    OriginNotAllowed,
    /// The agent doesn't advertise the `ssh` RPC capability — an older build,
    /// or one compiled without the `ssh-server` feature.
    Unsupported,
    /// The device is offline.
    Offline,
    /// The device is on the mesh but has no overlay address to dial, so there
    /// is nowhere to send the caller even though every gate passed.
    NoOverlayAddress,
    /// Per-(user, device) rate limit.
    RateLimited,
    /// The caller offered something other than a usable ed25519 public key.
    BadPublicKey,
}

impl SshDenyReason {
    /// One line an operator can act on. Deliberately names WHICH gate said no:
    /// "denied" without a reason turns a five-second config fix into a
    /// support ticket.
    pub fn message(self) -> &'static str {
        match self {
            Self::OrgDisabled => {
                "roomler SSH is disabled for this organization (an admin must enable it)"
            }
            Self::NoPermission => "you do not have the SSH_DEVICE permission in this organization",
            Self::DeviceDisabled => "this device does not accept SSH sessions (its policy is off)",
            Self::CallerNotAllowed => "this device's policy does not list you as an allowed caller",
            Self::OriginNotAllowed => {
                "the device you are calling from is not permitted to originate SSH sessions"
            }
            Self::Unsupported => {
                "this device's agent does not support roomler SSH (needs a build with the \
                 ssh-server feature)"
            }
            Self::Offline => "the device is offline",
            Self::NoOverlayAddress => {
                "the device has no overlay address — it is not on the mesh right now"
            }
            Self::RateLimited => "too many SSH requests for this device; try again shortly",
            Self::BadPublicKey => "the session public key is missing or not a usable ed25519 key",
        }
    }
}

/// One remote-execution attempt. Written on EVERY attempt, allowed or denied.
/// TTL-expired after 90 days like [`RemoteAuditEvent`].
///
/// `stdout` / `stderr` here are the REDACTED, truncated samples — the agent
/// sweeps its output for the agent token, `Bearer …`, and JWT-shaped strings
/// before anything leaves the host, so a secret echoed by a command never
/// reaches this collection.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// The device the command was aimed at.
    pub agent_id: ObjectId,
    /// The acting principal. For a CLI-originated request this is the
    /// originating device's `owner_user_id`.
    pub user_id: ObjectId,
    /// Set when the request came from a device's LocalAPI (`roomler exec`)
    /// rather than an authenticated HTTP caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_agent_id: Option<ObjectId>,
    /// Correlates the audit row with the wire `request_id`.
    pub request_id: String,
    /// `cli` | `ui` | `api`.
    pub source: String,
    pub shell: String,
    /// The command verbatim, as submitted.
    pub command: String,
    pub at: DateTime,
    /// `None` when the request never ran (see [`Self::denied`]) or the agent
    /// failed to report one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Refusal reason; `None` = the command ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<ExecDenyReason>,
    /// Redacted, truncated output sample (both streams, capped).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_sample: String,
    /// SHA-256 of the full redacted output, so a truncated sample can still
    /// be tied to what actually ran.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_sha256: String,
    #[serde(default)]
    pub output_bytes: u64,
    #[serde(default)]
    pub truncated: bool,
}

impl ExecAuditEvent {
    pub const COLLECTION: &'static str = "exec_audit";
    /// Cap on the persisted output sample. Keeps a busy fleet's audit
    /// collection bounded while retaining enough to read at a glance.
    pub const SAMPLE_BYTES: usize = 4096;
}

/// One roomler-SSH session REQUEST. Written on every attempt, granted or
/// refused. TTL-expired after 90 days like [`ExecAuditEvent`].
///
/// **This records the DECISION, not the session.** The server's involvement
/// ends when it hands back an address and a grant — the session itself rides
/// the overlay directly between the caller and the device, and the server
/// never observes it. So a row here means "a grant was issued", never "a
/// session happened", and there is deliberately no duration, exit code or
/// output: the design that keeps the server out of the data path is the same
/// design that stops it from auditing the data path. What lands on the DEVICE
/// (which grant authenticated, as whom, when) is the device's own log.
///
/// The refused rows are the load-bearing ones. Without them, someone probing
/// which devices will let them in leaves no trace at all — which is why every
/// exit from `agent_ssh::dispatch` funnels through one audit write rather than
/// each refusal site remembering to log itself.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SshAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// The device the session was aimed at.
    pub agent_id: ObjectId,
    /// The acting principal. For a device-originated request this is the
    /// originating device's `owner_user_id`.
    pub user_id: ObjectId,
    /// Set when the request came from a device's LocalAPI rather than an
    /// authenticated HTTP caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_agent_id: Option<ObjectId>,
    /// The minted grant, absent on refusal. Correlates this row with the
    /// device-side log line that redeemed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    /// `cli` (device LocalAPI leg) | `api` (authenticated HTTP caller).
    pub source: String,
    /// Display name of the principal, as it was pushed to the device.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub caller: String,
    /// Which identity the grant authorised — the field that makes this log
    /// worth reading, since it is the difference between a shell as the
    /// console user and a shell as SYSTEM/root. Absent on refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_mode: Option<String>,
    /// Bound placed on the session, in seconds. Absent on refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_secs: Option<u64>,
    pub at: DateTime,
    /// Refusal reason; `None` = a grant was issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<SshDenyReason>,
}

impl SshAuditEvent {
    pub const COLLECTION: &'static str = "ssh_audit";
}

/// A desired-config write, granted or refused (`docs/remote-config.md`).
///
/// The refused rows are the point, as in [`SshAuditEvent`]: an admin probing
/// which devices they can open exec on should not be able to do so silently.
///
/// ⚠️ A row records what was **ASKED FOR**, not what the device did. The
/// device may be offline, may not have opted in, and may refuse — so reading
/// these to answer "does this device have exec on?" is wrong in the same way
/// reading `ssh_audit` to answer "how long was that session?" is wrong. The
/// device's own heartbeat is the only truth for applied state.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ConfigAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// The device the change was aimed at.
    pub agent_id: ObjectId,
    /// Who asked.
    pub user_id: ObjectId,
    pub at: DateTime,
    /// The requested state. Serialised whole so the row explains itself
    /// without a join against a row that has since been overwritten.
    pub requested: DesiredConfig,
    /// `None` on a granted write; the [`ConfigDenyReason`] wire string on a
    /// refusal.
    ///
    /// [`ConfigDenyReason`]: https://docs.rs/  (api crate, routes::remote_config)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<String>,
}

impl ConfigAuditEvent {
    pub const COLLECTION: &'static str = "config_audit";
}

/// FR-40 — a key-rotation order, dispatched or refused. The SERVER's own
/// decision (authoritative), as opposed to the device's
/// [`KeyRotationReport`] (a claim). Same discipline as [`ConfigAuditEvent`]:
/// both arms land here from ONE call site, so a new refusal cannot forget to
/// audit itself, and a row records what was ORDERED, never what the device
/// did with it.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KeyRotationAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub agent_id: ObjectId,
    /// Who asked.
    pub user_id: ObjectId,
    pub at: DateTime,
    pub request_id: String,
    /// `pushed` (a live socket took it) or `queued` (the device was offline
    /// and will be ordered on connect). `None` on a refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<String>,
    /// The refusal's wire string; `None` when the order went out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied: Option<String>,
}

impl KeyRotationAuditEvent {
    pub const COLLECTION: &'static str = "key_rotation_audit";
}

/// What a device reported doing inside an SSH session (P8).
///
/// Deliberately coarse. Recording session CONTENT — a pty byte stream — would
/// mean shipping whatever the operator typed, including passwords typed into
/// `sudo` or `mysql -p`, off the host and into the server, which is the exact
/// property [`SshAuditEvent`]'s doc says this system does not have. These
/// answer "what was run", not "what was on the screen".
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SshActivityKind {
    /// A session authenticated. The envelope for everything that follows.
    SessionOpen,
    /// A session ended.
    SessionClose,
    /// A one-shot `ssh <node> <cmd>`. `detail` is the command, `exit_code`
    /// its status.
    Exec,
    /// An interactive shell started. **Its contents are not recorded** — this
    /// row exists so a reader can see that a shell happened at all.
    Shell,
    /// The SFTP subsystem started. File operations happen inside
    /// `sftp-server` and are not visible here.
    Sftp,
    /// A `direct-tcpip` forward was requested. `detail` is `host:port`,
    /// `allowed` says whether the device's `forward_acl` permitted it.
    Forward,
}

/// One thing a device reported doing inside an SSH session (P8).
///
/// ⚠️ **These rows are REPORTED BY THE DEVICE, not observed by the server** —
/// which is why they live in their own collection rather than alongside
/// [`SshAuditEvent`]. An audit row is the server's own decision and is
/// authoritative; an activity row is a claim by a host that could be
/// compromised or simply have reporting switched off. Mixing them would leave
/// a reader unable to tell which is which.
///
/// ⚠️ **Absence of rows is not evidence of inactivity.** A device with
/// `ssh_activity_log = false` (the default) reports nothing at all, and that
/// is indistinguishable from a device nobody used. Read this log together
/// with `ssh_audit`, which records every *grant* regardless of what the device
/// chooses to say afterwards.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SshActivityEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// From the authenticated WS, never from the frame — a device must not be
    /// able to write rows against another tenant.
    pub tenant_id: ObjectId,
    /// Likewise from the connection: the device doing the reporting.
    pub agent_id: ObjectId,
    /// Correlates with the [`SshAuditEvent`] that authorised the session, and
    /// therefore with the authoritative `user_id`. `None` for a key-list
    /// session, which no grant backs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    /// Principal as the DEVICE saw it. Unverified; join on `grant_id` for the
    /// server's own record of who asked.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub caller: String,
    pub kind: SshActivityKind,
    /// The command for `Exec`, `host:port` for `Forward`. Redacted and
    /// length-capped on the device before it leaves the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// `false` when the DEVICE refused the action — a forward its
    /// `forward_acl` did not permit. Those are the rows worth reading.
    pub allowed: bool,
    /// Stamped by the SERVER. A device clock is not evidence.
    pub at: DateTime,
}

impl SshActivityEvent {
    pub const COLLECTION: &'static str = "ssh_activity";

    /// Longest `detail` a device may report. A command line is the only
    /// attacker-influenced field here, and an unbounded one would let a single
    /// session bloat the collection.
    pub const MAX_DETAIL: usize = 512;
}

// ────────────────────────────────────────────────────────────────────────────
// Session
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Pending,
    AwaitingConsent,
    Negotiating,
    Active,
    Closed,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    ControllerHangup,
    AgentHangup,
    UserDenied,
    ConsentTimeout,
    /// FR-27 — the device could raise no consent prompt at all: no desktop
    /// session, no companion, no native overlay. Distinct from
    /// [`Self::ConsentTimeout`] because nobody was ever asked, so the useful
    /// advice is "give that host a prompt surface, or pick email/push", not
    /// "try again and answer it".
    NoPromptSurface,
    AgentDisconnect,
    AdminTerminated,
    IdleTimeout,
    Error,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SessionStats {
    pub bytes_sent: u64,
    pub bytes_recv: u64,
    pub peak_fps: f32,
    pub avg_rtt_ms: f32,
    pub keyframe_requests: u32,
    pub input_events: u64,
    /// P8 Phase 4 — cumulative seconds this session's shared pipeline
    /// served ≥1 follower. `default` so pre-Phase-4 rows deserialize.
    #[serde(default)]
    pub shared_seconds: u64,
    /// … of which the viewers' dials were NOT all equal (the SVC
    /// go/no-go dataset — see the signalling twin's doc).
    #[serde(default)]
    pub mixed_dial_seconds: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoteSession {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub controller_user_id: ObjectId,
    #[serde(default)]
    pub watchers: Vec<ObjectId>,
    pub permissions: Permissions,
    pub phase: SessionPhase,
    pub created_at: DateTime,
    pub started_at: Option<DateTime>,
    pub ended_at: Option<DateTime>,
    pub end_reason: Option<EndReason>,
    pub recording_url: Option<String>,
    #[serde(default)]
    pub stats: SessionStats,
}

impl RemoteSession {
    pub const COLLECTION: &'static str = "remote_sessions";
}

// ────────────────────────────────────────────────────────────────────────────
// Audit
// ────────────────────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditKind {
    /// Multi-user P3 — carries the ACTOR + effective grant (the plan's
    /// per-event attribution): with N concurrent sessions per agent, "who
    /// asked" is no longer derivable from the one live session. The same
    /// event now drives the `remote_sessions` row projection in the audit
    /// sink. Fields are serde-defaulted so pre-P3 rows still deserialize
    /// (defaults: zero ObjectId / empty name / default grant).
    SessionRequested {
        #[serde(default)]
        controller_user_id: ObjectId,
        #[serde(default)]
        controller_name: String,
        #[serde(default)]
        permissions: Permissions,
    },
    ConsentPrompted,
    ConsentGranted,
    ConsentDenied,
    ConsentTimedOut,
    SessionStarted,
    SessionEnded {
        reason: EndReason,
    },
    ClipboardWriteToHost {
        bytes: u32,
    },
    ClipboardReadFromHost {
        bytes: u32,
    },
    FileSentToHost {
        name: String,
        bytes: u64,
    },
    FileSentFromHost {
        name: String,
        bytes: u64,
    },
    KeyframeRequested,
    PermissionsChanged {
        permissions: Permissions,
    },
    WatcherJoined {
        user_id: ObjectId,
    },
    WatcherLeft {
        user_id: ObjectId,
    },
    Error {
        message: String,
    },
    /// An `ADMINISTRATOR` started this session via break-glass, skipping the
    /// device's consent mode. `reason` is operator-supplied and mandatory — the
    /// accountability record for a forced, unconsented session (docs §11.5).
    AdminOverride {
        reason: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RemoteAuditEvent {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub session_id: ObjectId,
    pub agent_id: ObjectId,
    pub tenant_id: ObjectId,
    pub at: DateTime,
    pub event: AuditKind,
}

impl RemoteAuditEvent {
    pub const COLLECTION: &'static str = "remote_audit";
}

// ────────────────────────────────────────────────────────────────────────────
// Agent crash report
// ────────────────────────────────────────────────────────────────────────────

/// Why the agent considers this a crash. Shared between the agent's
/// `crash_recorder` writer and the backend's ingest handler so a
/// future tag rename never silently breaks deserialisation.
///
/// Serialised as snake_case strings (`panic` / `watchdog_stall` /
/// `supervisor_detected`) — admin UI keys its chip-colour map off
/// these EXACT strings.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CrashReason {
    /// `std::panic::set_hook` fired in the worker process.
    Panic,
    /// `watchdog::force_exit_on_stall` was called — a registered
    /// pump's heartbeat gap exceeded its threshold (default 90 s).
    WatchdogStall,
    /// Windows SCM supervisor detected the worker process exited
    /// with a non-zero code (and the code wasn't `STALL_EXIT_CODE`,
    /// which is recorded at the watchdog site instead).
    SupervisorDetected,
}

/// Wire shape for the agent → roomler.ai crash-report upload AND the
/// on-disk sidecar the agent writes between crash + upload. `rename_
/// all = "camelCase"` so JS clients get `crashedAtUnix` etc. without
/// a translation step.
///
/// Size budget: 64 KiB total when JSON-serialised. The agent's
/// `crash_recorder::record` enforces this by trimming the
/// `log_tail` (oldest lines first) before write; the backend's
/// ingest route enforces it again with an 80 KiB body limit on the
/// HTTP request (small JSON overhead beyond the payload).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentCrashPayload {
    /// Unix seconds at the moment the crash was recorded ON THE
    /// AGENT. The backend stamps its own `reported_at` server-clock
    /// timestamp on ingest; admin UI shows both so clock-skewed
    /// hosts are visible.
    pub crashed_at_unix: i64,
    pub reason: CrashReason,
    /// One-line summary suitable for a list-view row (panic
    /// message, "pumps stalled (signaling=120s)", or "worker exit
    /// code 134"). May carry a trailing `[scrubbed N tokens]`
    /// marker if the scrub pipeline redacted credentials from the
    /// summary.
    pub summary: String,
    /// Last ~200 lines of the rolling agent log, after credential
    /// scrubbing. Truncated with a leading `[…log truncated to fit
    /// 64 KiB envelope…]\n` marker if the original tail wouldn't
    /// fit the size budget.
    pub log_tail: String,
    /// `env!("CARGO_PKG_VERSION")` at crash time.
    pub agent_version: String,
    /// `"windows"` / `"linux"` / `"macos"` — same string surface as
    /// `OsKind::serialize` would emit but kept as a plain String
    /// here so the payload doesn't depend on the OsKind enum
    /// position.
    pub os: String,
    /// Hostname at crash time.
    pub hostname: String,
    /// OS process id of the crashed worker (or supervisor, for the
    /// supervisor-detected branch).
    pub pid: u32,
    /// rc.51: how many crash sidecars were rate-limit-suppressed
    /// (`crash_recorder` 1/60 s throttle) between the previous
    /// successfully-written sidecar and this one. `0` in steady
    /// state; a high value means a tight crash-loop was in progress
    /// and most of its iterations went unrecorded — so this one
    /// sidecar represents `1 + suppressed_since_last` crashes.
    /// `#[serde(default)]` so pre-rc.51 sidecars (which lack the
    /// field) still deserialise.
    #[serde(default)]
    pub suppressed_since_last: u32,
}

/// Server-side persisted form of an agent crash report. The MongoDB
/// collection is `agent_crashes`; admin UI fetches via the protected
/// `GET /api/tenant/{tenant_id}/agent/{agent_id}/crash` endpoint.
///
/// Fields:
/// - `_id` / `tenant_id` / `agent_id` — server-attributed (resolved
///   from the agent JWT at ingest time).
/// - `reported_at` — server clock at ingest. Distinct from the
///   payload's `crashed_at_unix` (agent clock) so clock-skewed hosts
///   are visible in the admin UI.
/// - Everything else is flattened from [`AgentCrashPayload`] via
///   `#[serde(flatten)]`. The MongoDB BSON uses camelCase keys
///   matching the wire shape — no rename for the DB layer because
///   the payload's `rename_all = "camelCase"` carries through.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentCrashRecord {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub agent_id: ObjectId,
    pub reported_at: DateTime,
    #[serde(flatten)]
    pub payload: AgentCrashPayload,
}

impl AgentCrashRecord {
    pub const COLLECTION: &'static str = "agent_crashes";
}

// ────────────────────────────────────────────────────────────────────────────
// Overlay network (Tailscale-style L3 mesh)
// ────────────────────────────────────────────────────────────────────────────
//
// An overlay node is the unifying layer above `Agent` and `TunnelClient`:
// either kind of host can join a per-tenant virtual LAN, get a stable
// overlay IP, and reach any permitted peer at L3 over WireGuard. The two
// underlying collections keep their distinct lifecycles/audiences; an
// `OverlayNode` references one of them via [`NodeRef`] and adds the
// overlay-specific identity (WG pubkey + overlay IP + endpoints).

/// Which underlying host an [`OverlayNode`] is. Adjacently tagged so the
/// BSON/JSON shape is `{"kind":"agent","id":<oid>}` — mirrors the
/// `PolicySubject` / `PolicyTarget` style. The `id` stays a native
/// ObjectId for DB rows (Mongo indexes rely on native encoding); the
/// wire/netmap exposes nodes by their `overlay_nodes._id`, not by this.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeRef {
    Agent {
        #[serde(rename = "id")]
        agent_id: ObjectId,
    },
    TunnelClient {
        #[serde(rename = "id")]
        tunnel_client_id: ObjectId,
    },
}

/// One member of a tenant's overlay network. Keyed for rehydrate-on-
/// re-enroll by `(tenant_id, machine_id)` exactly like [`Agent`] /
/// [`TunnelClient`], so a re-joining host keeps its overlay IP (and may
/// register a rotated WG key). The WG **private** key never leaves the
/// node; only `wg_public_key` is stored + distributed in the netmap.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OverlayNode {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    pub node_ref: NodeRef,
    pub network_id: ObjectId,
    /// Rehydrate key — carried from the underlying agent/tunnel-client so
    /// a re-join finds the existing row (and its leased overlay IP).
    pub machine_id: String,
    /// Human-facing node name — denormalized from the underlying
    /// [`Agent`]/[`TunnelClient`] `name` at join, sanitized to a DNS label and
    /// made unique per network (collisions get a `-2`/`-3` suffix). This is the
    /// MagicDNS authority and the netmap's `name`. Empty on rows created before
    /// Phase 0 (Tailscale-style names).
    #[serde(default)]
    pub name: String,
    /// Leased overlay address, e.g. `"100.64.0.7"`. Stable for as long as the
    /// row is LIVE. On release (device removed from the fleet) the row is
    /// tombstoned and the host number goes back to `OverlayNetwork.free_hosts`
    /// for reuse — the address is KEPT here as the forensic record of who held
    /// it, which matters precisely because addresses now recycle. The
    /// `(tenant_id, network_id, overlay_ip)` unique index is scoped to live
    /// rows, so a tombstone holds nothing.
    pub overlay_ip: String,
    /// base64-encoded Curve25519 public key (WireGuard static key).
    pub wg_public_key: String,
    /// Bumped on key rotation (Phase 5). `0` at first join.
    #[serde(default)]
    pub key_epoch: u32,
    /// Current connectivity candidates (srflx / relay), as `host:port`
    /// strings the peer can dial. REPLACED on each `rc:overlay.endpoints`
    /// trickle from the relay coordinator.
    #[serde(default)]
    pub endpoints: Vec<String>,
    /// rc.135 — DIRECT LAN candidates, set from the agent's JOIN (kept in a
    /// SEPARATE bucket so the relay-endpoint trickle — which REPLACES
    /// `endpoints` — can't clobber them). The netmap a peer receives unions
    /// `lan_endpoints ∪ endpoints` so a same-subnet peer can always find the
    /// LAN address and go direct. (Field 2026-06-27: the trickle stripped
    /// `192.168.68.x` from nodes that had allocated a relay, forcing every
    /// peer onto the relay path.) Refreshed on each (re)join, so a DHCP IP
    /// change is picked up.
    #[serde(default)]
    pub lan_endpoints: Vec<String>,
    /// NAT-traversal Phase B — the node's SERVER-REFLEXIVE (srflx) candidates:
    /// its public `ip:port` as seen through its NAT, discovered by the node
    /// querying STUN on its own traffic sockets and trickled up via
    /// `rc:overlay.srflx`. A THIRD bucket, separate from `lan_endpoints` (public
    /// NIC) and `endpoints` (relay), so no trickle clobbers another's provenance
    /// (CC2). Surfaced VERBATIM in the netmap as `NetmapPeer.srflx_endpoints` so
    /// a peer behind a different NAT can dial this node directly (1:1 / cone NAT
    /// only). Reset to empty on each (re)join — the node re-gathers + re-trickles
    /// fresh srflx per connection, so a stale mapping never lingers.
    #[serde(default)]
    pub srflx_endpoints: Vec<String>,
    /// NAT-traversal Phase C — the node's probed NAT mapping type (`"cone"` /
    /// `"symmetric"`), trickled alongside its srflx via `rc:overlay.srflx` and
    /// surfaced as `NetmapPeer.srflx_nat`. A dialer skips the punch only when
    /// BOTH ends are `"symmetric"`. `None` = unknown (attempted, never skipped).
    /// Reset on each (re)join with `srflx_endpoints` — a prior session's NAT
    /// class is meaningless after a roam (A8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srflx_nat: Option<String>,
    /// Phase B (overlay v3) — the node's MEASURED capability vector
    /// (`rc:overlay.netcheck`), surfaced as `NetmapPeer.caps` behind the
    /// freshness gate on [`Self::caps_measured_at`]. Reset on re-join with
    /// the srflx bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps: Option<crate::signaling::CapVectorWire>,
    /// Receipt stamp for [`Self::caps`] — the freshness gate's input
    /// (vectors older than 3× the measurement cadence are surfaced as
    /// absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caps_measured_at: Option<bson::DateTime>,
    /// Dialer honesty (2026-08-16) — whether this node can raw-UDP-dial
    /// arbitrary relay-band ports (the single-relay DIALER's job), trickled
    /// with `rc:overlay.srflx` and surfaced as `NetmapPeer.udp_dialer_ok`.
    /// `Some(false)` = proved it can't (dialer-role convictions against ≥2
    /// distinct peers); `None` = pre-honesty agent (legacy role inputs).
    /// Reset on each (re)join with `srflx_endpoints`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_dialer_ok: Option<bool>,
    /// Preferred relay region/home, if any (Phase 5 multi-relay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_home: Option<String>,
    /// C4 stage 2 (PR-B) — the node's STANDING warm TURN allocation's relayed
    /// address (`worker-ip:port`), mirrored from its agent's heartbeats while
    /// a leg is live (`$unset` while none is). The netmap builder reads THIS
    /// row — not the agents collection — so the field must live here to reach
    /// `NetmapPeer.warm_relay_endpoint`, where a single-relay dialer uses it
    /// as the pair-less dial fallback. Cleared on rejoin like `srflx_*`: a
    /// restarted runtime holds no leg until its warm arm re-establishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_relay_endpoint: Option<String>,
    /// rc.142 — the node advertised (on JOIN) that it can carry WG over a
    /// QUIC-over-TURN relay carrier. Echoed per-peer in the netmap so QUIC is
    /// only attempted when both ends support it (no silent QUIC/raw split).
    #[serde(default)]
    pub supports_quic: bool,
    /// Phase D — the node advertised (on JOIN) that it can run the v1
    /// single-relay carrier (one anchor allocation + a raw dialer). Echoed
    /// per-peer in the netmap so single-relay is only chosen when both ends
    /// support it (a mismatch would split the pair into anchor/dialer).
    #[serde(default)]
    pub supports_relay_single: bool,
    /// Phase D (DERP) — the node advertised (on JOIN) that it can carry WG over
    /// the pubkey-addressed `/derp` relay, the last-resort carrier for two
    /// BOTH-UDP-blocked peers. Echoed per-peer in the netmap so DERP is only
    /// chosen when both ends support it. Absent on a pre-DERP row ⇒ `false`.
    #[serde(default)]
    pub supports_derp: bool,
    /// P7 — the node advertised (on JOIN) that it honors the server's per-pair
    /// `rc:overlay.force_derp` escalation push. The broker only escalates a
    /// churning pair when BOTH ends carry this, so a mixed-version pair can
    /// never split tiers. Absent on a pre-P7 row ⇒ `false`.
    #[serde(default)]
    pub supports_forced_derp: bool,
    /// U2 — the node advertised (on JOIN) that it accepts a server-computed
    /// relay-tier verdict. The broker stamps a per-edge `relay_strategy` in
    /// the netmap only when BOTH ends carry this. Absent on a pre-U2 row ⇒
    /// `false` ⇒ the pair keeps the client-authoritative path.
    #[serde(default)]
    pub supports_server_relay_strategy: bool,
    /// Phase A (overlay v3) — the node advertised (on JOIN) the DERP
    /// always-on floor: its central `/derp` mux stays open + registered for
    /// the whole session. Echoed per-peer; the floor is gated on BOTH ends.
    /// Absent on a pre-floor row ⇒ `false`.
    #[serde(default)]
    pub supports_derp_floor: bool,
    /// Data-probe — the node's overlay engine answers the overlay-native echo
    /// probe inline (advertised on JOIN, echoed per-peer in the netmap so
    /// probers prefer the engine-guaranteed echo). Absent on an older row ⇒
    /// `false` ⇒ peers probe it with ICMP.
    #[serde(default)]
    pub supports_overlay_echo: bool,
    /// FR-19 — this node understands `rc:overlay.relay_session` /
    /// `rc:overlay.relay_revoke` and the `org-relay` verdict (advertised on
    /// join, echoed per-peer in the netmap). The server never pushes those to a
    /// node that has not said so. Absent on an older row ⇒ `false`.
    #[serde(default)]
    pub supports_org_relay: bool,
    /// Phase 1 — subnet CIDRs this node CLAIMS it can route for peers (from its
    /// `--advertise-routes` config, refreshed on each join). Untrusted until an
    /// admin approves; see `approved_routes`.
    #[serde(default)]
    pub advertised_routes: Vec<String>,
    /// Phase 1 — the admin-APPROVED subset of `advertised_routes`, distributed
    /// to peers as the netmap `routes`. Empty = this node routes nothing for
    /// anyone. An admin manages this via the overlay-route approval UI.
    ///
    /// P5: a default route `0.0.0.0/0` here marks this node as an approved
    /// exit node (clients infer exit-nodes from an approved `/0` in the
    /// netmap). `/0` may ONLY enter this list via the exit-node toggle
    /// (`set_exit_node`), never the per-CIDR approval grid — see
    /// `set_approved_routes`'s `/0` guard — so an admin can't accidentally
    /// route the whole internet with a mis-clicked checkbox.
    #[serde(default)]
    pub approved_routes: Vec<String>,
    /// P5 — admin has designated this node as an exit node (routes
    /// `0.0.0.0/0` for opted-in clients). Kept as an explicit flag for the
    /// admin UI + approval semantics; the DATA-PLANE signal a client keys
    /// off is still the approved `0.0.0.0/0` in the netmap. Toggling this
    /// on requires the node to have advertised `0.0.0.0/0` and adds it to
    /// `approved_routes`; toggling off removes it.
    #[serde(default)]
    pub is_exit_node: bool,
    pub status: AgentStatus,
    pub last_seen_at: DateTime,
    pub created_at: DateTime,
    pub updated_at: DateTime,
    pub deleted_at: Option<DateTime>,
}

impl OverlayNode {
    pub const COLLECTION: &'static str = "overlay_nodes";
}

/// P5 — the IPv4 default route. Its presence in a node's `approved_routes`
/// is the wire signal that the node is an approved exit node; it may only be
/// added via the exit-node toggle, never the per-CIDR approval grid.
pub const DEFAULT_ROUTE_V4: &str = "0.0.0.0/0";

/// IPAM authority for one tenant's overlay. One row per tenant. The allocator
/// prefers a RECYCLED host from `free_hosts` (returned when a device is removed
/// from the fleet) and otherwise hands out the next number from the monotonic
/// `next_host` cursor (atomic `$inc`). A lease is stable for as long as its node
/// row is live.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OverlayNetwork {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub tenant_id: ObjectId,
    /// CGNAT range per the Tailscale convention, e.g. `"100.64.0.0/10"`.
    pub cidr: String,
    /// Monotonic host cursor — the next host number to hand out. `1` for
    /// a fresh network (host `0` is the network address, reserved). Only
    /// bumped when [`free_hosts`](Self::free_hosts) is empty.
    pub next_host: u32,
    /// Host numbers RECYCLED from removed devices, oldest release first. A
    /// join pops the head before touching `next_host`, so a released address
    /// gets the longest possible cool-down before it is handed out again.
    ///
    /// `#[serde(default)]` is LOAD-BEARING: every `overlay_networks` row
    /// written before the release feature lacks this field, and without the
    /// default they would all fail to deserialize — taking the entire overlay
    /// down on the first `get_or_create`.
    #[serde(default)]
    pub free_hosts: Vec<u32>,
    /// Path MTU for the overlay. 1280 leaves headroom for the WG +
    /// carrier (UDP/relay) overhead under a 1500-byte underlay.
    pub mtu: u16,
    /// How strictly [`OverlayPolicy`] rows are applied for this tenant.
    ///
    /// `#[serde(default)]` is LOAD-BEARING for exactly the reason spelled out
    /// on [`free_hosts`](Self::free_hosts): every row written before the ACL
    /// feature lacks this field, and without the default they would all fail
    /// to deserialize and take the whole overlay down on the next
    /// `get_or_create`. The default is [`OverlayAclMode::Off`], so an existing
    /// mesh keeps its current behaviour until an admin opts in.
    #[serde(default)]
    pub acl_mode: OverlayAclMode,
    /// FR-19 gate 1 — the org's peer-relay switch. `#[serde(default)]` for the
    /// same reason as `acl_mode`: every pre-FR-19 row lacks it, and the default
    /// is [`PeerRelayMode::Off`] so nothing is minted until an admin opts in.
    #[serde(default)]
    pub peer_relay_mode: PeerRelayMode,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

impl OverlayNetwork {
    pub const COLLECTION: &'static str = "overlay_networks";
    /// Default tenant overlay range (CGNAT block, like Tailscale).
    pub const DEFAULT_CIDR: &'static str = "100.64.0.0/10";
    /// Default overlay MTU.
    pub const DEFAULT_MTU: u16 = 1280;
    /// Ceiling on [`free_hosts`](Self::free_hosts). A `u32` array element costs
    /// ~9 bytes of BSON, so the 16 MB document cap is ~1.7 M entries — this cap
    /// sits far below it. Past the cap a release is DROPPED and the host leaks
    /// out of a 4.2 M-address `/10`: a leak, never a conflict.
    pub const MAX_FREE_HOSTS: usize = 65_536;

    /// This network's dotted address for `host`. See [`overlay_ip`].
    pub fn host_ip(&self, host: u32) -> Option<String> {
        overlay_ip(&self.cidr, host)
    }

    /// The host number `ip` was leased from in this network. See [`overlay_host`].
    pub fn host_of_ip(&self, ip: &str) -> Option<u32> {
        overlay_host(&self.cidr, ip)
    }

    /// Multi-org P2a — the highest host ordinal this network's CIDR can
    /// lease. See [`cidr_max_host`]; `0` (nothing leasable) on a malformed
    /// CIDR so a bad row FAILS CLOSED at allocation instead of handing out
    /// unbounded addresses.
    pub fn max_host(&self) -> u32 {
        cidr_max_host(&self.cidr).unwrap_or(0)
    }
}

/// Multi-org P2a — the highest leaseable host ordinal for `cidr`:
/// `2^(32-prefix) - 2` (host 0 is the network address, the block's last
/// address is reserved by convention), `None` for a malformed CIDR or a
/// prefix ≥ 31 (no leasable hosts either way).
///
/// The forward-compat point: `allocate_host`'s cursor previously grew
/// UNBOUNDED — under tenant-block addressing (P2b hands tenants sub-blocks
/// of `100.64.0.0/10`, e.g. a `/22`) a busy tenant's cursor would have
/// silently walked into the NEIGHBOR tenant's block: the exact cross-tenant
/// address collision the blocks exist to kill. Every allocation is now
/// bounded by the network's OWN CIDR — behaviour-neutral for today's
/// `/10` tenants (max host 4 194 302, unreachable), binding the moment a
/// tenant's `cidr` becomes a real block.
pub fn cidr_max_host(cidr: &str) -> Option<u32> {
    let (_base, prefix) = cidr.split_once('/')?;
    let prefix: u8 = prefix.parse().ok()?;
    if prefix >= 31 {
        return None;
    }
    // 2^(32-prefix) - 2; prefix 0 would overflow the shift — cap at /1.
    let size: u64 = 1u64 << (32 - prefix.min(31) as u64);
    u32::try_from(size - 2).ok()
}

/// `base_cidr` + host number → dotted overlay IP. e.g.
/// `("100.64.0.0/10", 7) → "100.64.0.7"`. No `ipnet` needed — the host
/// number is added to the network base address as a `u32`.
///
/// The base is taken from the LEFT of the `/` (not the masked network
/// address). [`overlay_host`] inverts this by parsing the same way, and the
/// round-trip test in this module pins the pair together — do not "fix" one
/// to mask without the other.
///
/// Multi-org P2a: `host` is bounded by [`cidr_max_host`] — a host past the
/// block renders `None` instead of an address in the NEIGHBOR block.
pub fn overlay_ip(cidr: &str, host: u32) -> Option<String> {
    if host > cidr_max_host(cidr)? {
        return None;
    }
    let (base, _prefix) = cidr.split_once('/')?;
    let base: std::net::Ipv4Addr = base.parse().ok()?;
    let addr = std::net::Ipv4Addr::from(u32::from(base).checked_add(host)?);
    Some(addr.to_string())
}

/// Exact inverse of [`overlay_ip`] — recover the host number a dotted overlay
/// address was leased from, so a removed device's number can be returned to the
/// network's free pool.
///
/// `None` when `cidr`/`ip` are malformed, when `ip` sorts BELOW the base, or
/// (multi-org P2a) ABOVE the block's last leaseable host: both directions are
/// how a row leased under a DIFFERENT, since-changed CIDR is rejected instead
/// of poisoning the pool with a bogus host number.
pub fn overlay_host(cidr: &str, ip: &str) -> Option<u32> {
    let (base, _prefix) = cidr.split_once('/')?;
    let base: std::net::Ipv4Addr = base.parse().ok()?;
    let addr: std::net::Ipv4Addr = ip.parse().ok()?;
    let host = u32::from(addr).checked_sub(u32::from(base))?;
    if host > cidr_max_host(cidr)? {
        return None;
    }
    Some(host)
}

// ---------------------------------------------------------------------------
// Multi-org P2b — tenant-block addressing
// ---------------------------------------------------------------------------

/// The CGNAT root every tenant block is carved from.
pub const OVERLAY_BLOCK_ROOT: &str = "100.64.0.0/10";

/// Base address of [`OVERLAY_BLOCK_ROOT`] as a `u32`.
const OVERLAY_BLOCK_ROOT_BASE: u32 = u32::from_be_bytes([100, 64, 0, 0]);

/// The allocation grid: every block is an ALIGNED run of `/22`s. A `/22`
/// (1024 addresses, 1022 leasable) is the smallest unit handed out, so the
/// registry can enforce non-overlap with one integer per block.
pub const OVERLAY_BLOCK_SLOT_PREFIX: u8 = 22;

/// Addresses per slot (`2^(32-22)` = 1024).
pub const OVERLAY_BLOCK_SLOT_SIZE: u32 = 1 << (32 - OVERLAY_BLOCK_SLOT_PREFIX as u32);

/// The first slot the allocator may hand out — slot 64, i.e. `100.65.0.0`.
///
/// The whole of `100.64.0.0/16` (slots 0..63) is RESERVED for LEGACY tenants:
/// every network created before blocks existed carries `100.64.0.0/10` with
/// its host cursor seeded at 1, so all of them sit in `100.64.0.x` and grow
/// upward. Starting new blocks above that reserve means a carved tenant can
/// never collide with a legacy one, no matter how many devices the legacy
/// tenant has leased (65 534 before it would reach slot 64).
pub const OVERLAY_BLOCK_FIRST_SLOT: u32 = 64;

/// Number of slots in the `/10` (`2^(22-10)`).
pub const OVERLAY_BLOCK_SLOT_COUNT: u32 = 1 << (OVERLAY_BLOCK_SLOT_PREFIX as u32 - 10);

/// Largest block a tenant may be carved (a `/16`, 64 slots, 65 534 hosts).
pub const OVERLAY_BLOCK_MIN_PREFIX: u8 = 16;

/// Smallest block a tenant may be carved — one slot.
pub const OVERLAY_BLOCK_MAX_PREFIX: u8 = OVERLAY_BLOCK_SLOT_PREFIX;

/// Lifecycle of a registry row.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum OverlayBlockState {
    /// Currently backing a tenant's overlay network.
    #[default]
    Assigned,
    /// Freed by a renumber. The slots are NEVER handed out again: a device
    /// that missed the migration (offline, or a stale binary) still believes
    /// it holds an address in here, and re-issuing the range to a different
    /// tenant would hand that stale node a live neighbour's address. The row
    /// is kept as the forensic record of who held the range.
    Quarantined,
    /// An operator has judged a quarantined range safe to hand out again:
    /// old enough that any device would have reconnected and been
    /// renumbered many times over, and with no live node still holding an
    /// address inside it.
    ///
    /// Reclaimed rows are the allocator's **fallback, not its default**.
    /// Normal allocation stays strictly monotonic-upward — that is what
    /// makes non-overlap structural rather than locked — and only reaches
    /// for a reclaimed range when the monotonic path has genuinely run out
    /// of space above. So the risk this state carries is paid exactly when
    /// the alternative is "no address at all", and never before.
    Reclaimed,
}

/// One entry in the GLOBAL overlay block registry (multi-org P2b).
///
/// Blocks are allocated MONOTONICALLY upward from
/// [`OVERLAY_BLOCK_FIRST_SLOT`]: the allocator takes the highest `end_slot`
/// in the collection, rounds up to this block's alignment, and inserts with a
/// unique `slot`. Two racers either compute the SAME start (one loses the
/// unique index and retries against the winner's row) or compute
/// buddy-aligned, non-overlapping starts — so the registry never issues
/// overlapping ranges without needing a lock. Quarantined rows keep their
/// slots occupied, which is exactly what makes quarantine free.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OverlayBlock {
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    /// Offset from `100.64.0.0` in [`OVERLAY_BLOCK_SLOT_SIZE`] units.
    /// Globally unique — this is what makes overlap unrepresentable.
    pub slot: u32,
    /// How many contiguous slots this block spans (a power of two; the start
    /// is always a multiple of it).
    pub slots: u32,
    /// `slot + slots`, denormalized so the allocator's "highest end" probe is
    /// a single indexed `sort + limit 1` instead of a collection scan.
    pub end_slot: u32,
    /// The rendered range, e.g. `"100.65.0.0/22"`. This is copied to
    /// `OverlayNetwork.cidr` — the registry is the authority.
    pub cidr: String,
    /// FR-47 P5b — this block's POSITION in its network's block list, from 0.
    ///
    /// The ordinal space is the blocks concatenated in allocation order
    /// ([`BlockList`]), so the order has to be recoverable exactly — get it
    /// wrong and every ordinal above the misplaced block silently re-points at
    /// a different address.
    ///
    /// ⚠️ It is an explicit field because **neither obvious key works**.
    /// `slot` fails: [`OverlayBlockState::Reclaimed`] ranges are re-issued
    /// from BELOW the cursor, so a newly-allocated block can carry a lower
    /// slot than one allocated years earlier. `created_at` fails too, for the
    /// mirror reason — the reclaim path reuses the ROW, so its `created_at`
    /// is the date the range was first carved for a different tenant.
    ///
    /// `#[serde(default)]` reads every pre-P5b row as `0`, which is correct:
    /// before multi-block a network had exactly one assigned block.
    #[serde(default)]
    pub seq: u32,
    pub tenant_id: ObjectId,
    pub network_id: ObjectId,
    pub state: OverlayBlockState,
    /// Why the block was quarantined (renumber, block grow, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<DateTime>,
    pub created_at: DateTime,
    pub updated_at: DateTime,
}

impl OverlayBlock {
    pub const COLLECTION: &'static str = "overlay_blocks";

    /// Highest host ordinal this block can lease.
    pub fn max_host(&self) -> u32 {
        cidr_max_host(&self.cidr).unwrap_or(0)
    }
}

/// Slots consumed by a block of `prefix`, or `None` when the prefix is outside
/// the supported band ([`OVERLAY_BLOCK_MIN_PREFIX`] ..=
/// [`OVERLAY_BLOCK_MAX_PREFIX`]). Anything longer than a `/22` would have to
/// sub-divide a slot, which the registry deliberately does not model.
pub fn block_slots_for_prefix(prefix: u8) -> Option<u32> {
    if !(OVERLAY_BLOCK_MIN_PREFIX..=OVERLAY_BLOCK_MAX_PREFIX).contains(&prefix) {
        return None;
    }
    Some(1 << (OVERLAY_BLOCK_SLOT_PREFIX as u32 - prefix as u32))
}

/// The rendered CIDR for an aligned run of `slots` slots starting at `slot`.
/// `None` when the run is unaligned, empty, or would leave the `/10`.
pub fn block_cidr_for_slot(slot: u32, slots: u32) -> Option<String> {
    if slots == 0 || !slots.is_power_of_two() || !slot.is_multiple_of(slots) {
        return None;
    }
    let end = slot.checked_add(slots)?;
    if end > OVERLAY_BLOCK_SLOT_COUNT {
        return None;
    }
    let base = OVERLAY_BLOCK_ROOT_BASE.checked_add(slot.checked_mul(OVERLAY_BLOCK_SLOT_SIZE)?)?;
    let prefix = OVERLAY_BLOCK_SLOT_PREFIX as u32 - slots.trailing_zeros();
    Some(format!(
        "{}/{}",
        std::net::Ipv4Addr::from(base),
        prefix as u8
    ))
}

/// The lowest aligned start for a `slots`-wide block that begins at or after
/// `after`. Alignment (start is a multiple of the width) is what makes two
/// concurrently-computed allocations either identical or disjoint.
pub fn block_align_slot(after: u32, slots: u32) -> u32 {
    if slots == 0 {
        return after;
    }
    after.div_ceil(slots) * slots
}

// ---------------------------------------------------------------------------
// FR-47 P5a — an org's address space as a LIST of blocks
// ---------------------------------------------------------------------------

/// An organization's overlay address space: an ordered, **append-only** list
/// of blocks addressed by one continuous ordinal space.
///
/// Ordinals run `1..=capacity()` across the whole list — `1..=1022` in block 0,
/// `1023..=2044` in block 1, and so on. That concatenation is what lets a
/// network outgrow its first block **without renumbering a single device**:
/// growth appends a block at the tail and every existing ordinal keeps meaning
/// exactly the address it already meant.
///
/// ⚠️ **Append-only is load-bearing, not a convention.** Every entry in
/// `OverlayNetwork.free_hosts` is an ordinal, and so is every address a live
/// node holds. Insert or remove a block anywhere but the tail and every
/// ordinal above it silently re-points at a different address — which is the
/// one failure this whole FR exists to prevent. The invariant holds for free
/// today because blocks are *quarantined*, never removed
/// ([`OverlayBlockState::Quarantined`]).
///
/// A single-block list behaves **identically** to the bare
/// [`overlay_ip`] / [`overlay_host`] pair it generalizes; that equivalence is
/// pinned by test, because it is what makes multi-block safe to ship behind a
/// flag on a live fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockList {
    /// Blocks in allocation order. Order IS the ordinal layout.
    cidrs: Vec<String>,
}

impl BlockList {
    /// Build from blocks in allocation order (oldest first).
    ///
    /// Blocks that cannot lease anything are dropped rather than kept as
    /// zero-width entries: a `/31` in the middle of the list would contribute
    /// no ordinals but would still be a position someone could later "fix"
    /// into a real block, shifting everything above it.
    pub fn new<I, S>(cidrs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            cidrs: cidrs
                .into_iter()
                .map(Into::into)
                .filter(|c| cidr_max_host(c).is_some_and(|m| m > 0))
                .collect(),
        }
    }

    /// The single-block case — exactly today's behaviour.
    pub fn single(cidr: &str) -> Self {
        Self::new([cidr])
    }

    /// Blocks in allocation order.
    pub fn cidrs(&self) -> &[String] {
        &self.cidrs
    }

    /// Total leasable ordinals across every block.
    pub fn capacity(&self) -> u32 {
        self.cidrs
            .iter()
            .filter_map(|c| cidr_max_host(c))
            .fold(0u32, |a, b| a.saturating_add(b))
    }

    /// The address for a whole-space ordinal, or `None` past the end.
    pub fn ip_for_ordinal(&self, ordinal: u32) -> Option<String> {
        if ordinal == 0 {
            return None; // host 0 is the network address, never leased
        }
        let mut remaining = ordinal;
        for cidr in &self.cidrs {
            let width = cidr_max_host(cidr)?;
            if remaining <= width {
                return overlay_ip(cidr, remaining);
            }
            remaining -= width;
        }
        None
    }

    /// The whole-space ordinal an address maps to, or `None` when no block in
    /// this list contains it.
    pub fn ordinal_for_ip(&self, ip: &str) -> Option<u32> {
        let mut base = 0u32;
        for cidr in &self.cidrs {
            let width = cidr_max_host(cidr)?;
            // `overlay_host` returns Some(0) for the network address itself,
            // which is not a lease — skip it rather than reporting ordinal 0.
            if let Some(h) = overlay_host(cidr, ip)
                && h > 0
            {
                return Some(base + h);
            }
            base += width;
        }
        None
    }

    /// The block that actually contains `ip`.
    ///
    /// This is what a netmap sends a node as its own `cidr`: an agent derives
    /// its TUN netmask and its subnet-router NAT scope from that string, and
    /// both are only correct for the block the node's OWN address lives in.
    pub fn cidr_for_ip(&self, ip: &str) -> Option<&str> {
        self.cidrs
            .iter()
            .find(|c| overlay_host(c, ip).is_some())
            .map(|c| c.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-51 — a device row written before the ephemeral fields existed must
    /// deserialise PERMANENT. This is the property that makes enabling the
    /// reaper safe against the existing fleet: every pre-FR-51 row reads
    /// `ephemeral: false`, so the reaper's predicate can never match it.
    #[test]
    fn pre_fr51_agent_row_deserialises_permanent() {
        let now = bson::DateTime::now();
        // The minimal shape of a fielded row: none of the serde-defaulted
        // fields present, and in particular neither `ephemeral` nor
        // `ephemeral_ttl_secs`.
        let doc = bson::doc! {
            "tenant_id": ObjectId::new(),
            "owner_user_id": ObjectId::new(),
            "name": "old-host",
            "machine_id": "m-1",
            "os": "linux",
            "agent_version": "0.4.0",
            "agent_token_hash": "",
            "status": "offline",
            "last_seen_at": now,
            "created_at": now,
            "updated_at": now,
            "deleted_at": bson::Bson::Null,
        };
        let a: Agent = bson::from_document(doc).expect("pre-FR-51 row must deserialise");
        assert!(!a.ephemeral, "an old row must never read as ephemeral");
        assert_eq!(a.ephemeral_ttl_secs, None);
    }

    /// FR-19 — the `relay-server` verb: spelling locked (a rename silently
    /// un-advertises the feature fleet-wide), and only ever equality-matched —
    /// a future `relay` verb must not be implied by it, nor it by `relay`.
    #[test]
    fn relay_server_verb_is_locked_and_only_equality_matched() {
        assert_eq!(RpcCap::RelayServer.wire(), "relay-server");
        assert!(matches!(
            RpcCap::from_wire("relay-server"),
            Some(RpcCap::RelayServer)
        ));
        assert!(RpcCap::from_wire("relay").is_none());
        assert!(RpcCap::from_wire("relay-server-x").is_none());
        assert!(RpcCap::from_wire("relay-serve").is_none());
        assert!(RpcCap::ALL.iter().any(|c| c.wire() == "relay-server"));
    }

    /// FR-19 gate 4: **no `relay_*` key may ever be server-pushable.**
    ///
    /// Matches on the PREFIX, not on a name, and that is the whole point. A
    /// per-name assertion would pass forever while covering nothing new, and
    /// this surface is going to grow (`relay_max_sessions`,
    /// `relay_static_endpoints`, …).
    ///
    /// ⚠️ The struct literal below is spelled out in full **on purpose** — no
    /// `..Default::default()`. Every field is populated so the serialised form
    /// cannot pass merely because a value was `None` and got skipped; and,
    /// more importantly, adding any field to [`DesiredConfig`] makes this test
    /// stop compiling, which forces whoever adds it to come here and read why
    /// a `relay_*` key must not be the field they are adding. A tolerant
    /// literal would let that change land silently.
    #[test]
    fn no_relay_key_is_server_pushable_via_desired_config() {
        let full = DesiredConfig {
            exec_enabled: Some(true),
            ssh_enabled: Some(true),
            ssh_authorized_keys: Some(vec!["ssh-ed25519 AAAA".into()]),
            ssh_account_mode: Some("console_user".into()),
            ssh_port: Some(2222),
            revision: 7,
            updated_by: None,
            updated_at: None,
        };
        let json = serde_json::to_string(&full).unwrap();
        assert!(
            !json.contains("relay_"),
            "a relay_* key reached DesiredConfig -- that is gate 4 becoming \
             server-settable, which is the one move FR-19's design exists to \
             prevent: {json}"
        );
        // And the round trip cannot smuggle one in either.
        let back: DesiredConfig = serde_json::from_str(
            r#"{"exec_enabled":true,"relay_server_enabled":true,"relay_server_port":9,"revision":1}"#,
        )
        .expect("unknown keys must be ignored, not fail the frame");
        assert_eq!(back.exec_enabled, Some(true));
        assert!(
            !serde_json::to_string(&back).unwrap().contains("relay_"),
            "a relay_* key survived a decode/encode round trip"
        );
    }

    /// WIRE LOCK. These exact strings are what every deployed agent already
    /// sends and what the server gates on; changing one does not fail loudly,
    /// it silently makes every existing device look like it lacks the feature.
    /// Spelled out literally rather than derived, so a rename has to be a
    /// deliberate edit here.
    #[test]
    fn rpc_cap_wire_strings_are_locked() {
        assert_eq!(RpcCap::Exec.wire(), "exec");
        assert_eq!(RpcCap::Originate.wire(), "originate");
        assert_eq!(RpcCap::Ssh.wire(), "ssh");
        assert_eq!(RpcCap::SshConsent.wire(), "ssh-consent");
        assert_eq!(RpcCap::Config.wire(), "config");
        assert_eq!(RpcCap::ConfigReport.wire(), "config-report");
        assert_eq!(RpcCap::KeyRotate.wire(), "key-rotate");
    }

    /// Every prefix relationship between verbs is a KNOWN one.
    ///
    /// A blanket "no verb is a prefix of another" would be false and always
    /// has been: `ssh`/`ssh-consent` is deliberate, and `config`/`config-report`
    /// now is too. Both encode the same idea — "understands the feature" and
    /// "actually does the thing" are different questions — and both are why
    /// matching is equality everywhere.
    ///
    /// So the claim worth locking is not "there are none" but "there are
    /// exactly these". A THIRD pair appearing by accident (say someone adds
    /// `exec-batch`) fails here and has to be looked at, because each such
    /// pair is one more place a sloppy `starts_with` could be accidentally
    /// right on the devices that already exist.
    #[test]
    fn the_only_prefix_related_verbs_are_the_deliberate_ones() {
        const KNOWN: [(RpcCap, RpcCap); 2] = [
            (RpcCap::Ssh, RpcCap::SshConsent),
            (RpcCap::Config, RpcCap::ConfigReport),
        ];
        for a in RpcCap::ALL {
            for b in RpcCap::ALL {
                if a == b || !b.wire().starts_with(a.wire()) {
                    continue;
                }
                assert!(
                    KNOWN.contains(&(a, b)),
                    "`{}` is an UNPLANNED prefix of `{}` — either rename it or \
                     add the pair to KNOWN and write the equality-matching test \
                     that goes with it",
                    a.wire(),
                    b.wire()
                );
            }
        }
    }

    /// The `SshConsent` shape, recurring exactly as its doc predicted.
    ///
    /// Agents rc.457/rc.458 shipped `config` and report NOTHING back — they
    /// apply a pushed config in silence. A matcher using `starts_with` (or a
    /// design that folded the report into `config`) would mark every one of
    /// them as reporting, and the dashboard would wait forever for an answer
    /// that was never going to come, while showing the operator a state it
    /// had no evidence for.
    #[test]
    fn config_does_not_imply_config_report() {
        let rc458_era = AgentCaps {
            rpc: vec!["exec".into(), "originate".into(), "config".into()],
            ..Default::default()
        };
        assert!(rc458_era.has_rpc(RpcCap::Config));
        assert!(
            !rc458_era.has_rpc(RpcCap::ConfigReport),
            "an rc.458-era agent understands a pushed config but never reports \
             on it — it must NOT read as report-capable"
        );

        let both = AgentCaps {
            rpc: vec!["config".into(), "config-report".into()],
            ..Default::default()
        };
        assert!(both.has_rpc(RpcCap::Config) && both.has_rpc(RpcCap::ConfigReport));
    }

    #[test]
    fn all_entries_are_unique_and_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for cap in RpcCap::ALL {
            assert!(seen.insert(cap.wire()), "duplicate wire string {cap:?}");
            assert_eq!(RpcCap::from_wire(cap.wire()), Some(cap));
        }
        assert_eq!(seen.len(), RpcCap::ALL.len());
        assert_eq!(RpcCap::from_wire("no-such-verb"), None);
    }

    /// ⚠️ `ssh` is a PREFIX of `ssh-consent`, and the difference between them
    /// is the difference between "runs an SSH server" and "actually honours
    /// `consent_mode`". A matcher using `starts_with`/`contains` instead of
    /// equality would make every ssh-capable agent look consent-capable — and
    /// re-introduce exactly the lie P5d removed, on every device in the field
    /// at once. Lock the distinction.
    #[test]
    fn ssh_does_not_imply_ssh_consent() {
        let ssh_only = AgentCaps {
            rpc: vec!["exec".into(), "originate".into(), "ssh".into()],
            ..Default::default()
        };
        assert!(ssh_only.has_rpc(RpcCap::Ssh));
        assert!(
            !ssh_only.has_rpc(RpcCap::SshConsent),
            "an rc.419-era agent must NOT read as consent-capable"
        );

        let both = AgentCaps {
            rpc: vec!["ssh".into(), "ssh-consent".into()],
            ..Default::default()
        };
        assert!(both.has_rpc(RpcCap::Ssh) && both.has_rpc(RpcCap::SshConsent));
    }

    /// Forward compatibility: a NEWER agent may advertise verbs this build has
    /// never heard of. An additive string list is only additive if an older
    /// reader ignores what it does not recognise instead of choking.
    #[test]
    fn unknown_verbs_are_ignored_not_fatal() {
        let future = AgentCaps {
            rpc: vec!["exec".into(), "quantum-teleport".into()],
            ..Default::default()
        };
        assert!(future.has_rpc(RpcCap::Exec));
        assert!(!future.has_rpc(RpcCap::Ssh));
    }

    /// A pre-Fleet-RPC agent sends no `rpc` key at all; every gate must read
    /// false rather than defaulting open.
    ///
    /// The fixture is a ROUND TRIP rather than hand-written JSON: `rpc` is
    /// `skip_serializing_if = "Vec::is_empty"`, so an empty one genuinely
    /// disappears from the wire — which is exactly the old-agent shape — and
    /// deriving it this way cannot drift as other fields come and go.
    #[test]
    fn absent_rpc_list_advertises_nothing() {
        let wire = serde_json::to_string(&AgentCaps::default()).unwrap();
        assert!(
            !wire.contains("\"rpc\""),
            "an empty rpc list must not reach the wire at all: {wire}"
        );
        let old: AgentCaps = serde_json::from_str(&wire).unwrap();
        for cap in RpcCap::ALL {
            assert!(!old.has_rpc(cap), "{cap:?} must not be implied by silence");
        }
    }

    /// FR-77 — the cell vocabulary follows the `RpcCap` contract: every wire
    /// string is unique and round-trips, and the spellings are LOCKED because
    /// changing one silently un-advertises a cell on every deployed agent.
    #[test]
    fn video_cell_vocabulary_is_unique_locked_and_round_trips() {
        assert_eq!(
            VideoCodec::ALL.map(VideoCodec::wire),
            ["h264", "hevc", "av1", "vp9"]
        );
        assert_eq!(
            VideoBackend::ALL.map(VideoBackend::wire),
            [
                "nvenc",
                "qsv",
                "amf",
                "videotoolbox",
                "vaapi",
                "mf",
                "openh264",
                "libvpx"
            ]
        );
        assert_eq!(
            ChromaFormat::ALL.map(ChromaFormat::wire),
            ["yuv420", "yuv444"]
        );
        for c in VideoCodec::ALL {
            assert_eq!(VideoCodec::from_wire(c.wire()), Some(c));
        }
        for b in VideoBackend::ALL {
            assert_eq!(VideoBackend::from_wire(b.wire()), Some(b));
        }
        for c in ChromaFormat::ALL {
            assert_eq!(ChromaFormat::from_wire(c.wire()), Some(c));
        }
        assert_eq!(
            VideoCodec::from_wire("h265"),
            None,
            "the wire name is hevc, not h265"
        );
    }

    /// Every FFmpeg name the agent's dispatch tables carry must parse, and a
    /// name this build does not know must NOT parse — that is how a backend
    /// added to a table without a vocabulary entry fails in a test instead
    /// of shipping under a name no viewer understands.
    #[test]
    fn ffmpeg_names_split_into_codec_and_backend() {
        use VideoBackend as B;
        use VideoCodec as C;
        for (name, want) in [
            ("hevc_nvenc", (C::Hevc, B::Nvenc)),
            ("h264_qsv", (C::H264, B::Qsv)),
            ("av1_amf", (C::Av1, B::Amf)),
            ("vp9_qsv", (C::Vp9, B::Qsv)),
            ("h264_videotoolbox", (C::H264, B::VideoToolbox)),
            ("hevc_vaapi", (C::Hevc, B::Vaapi)),
        ] {
            assert_eq!(B::from_ffmpeg_name(name), Some(want), "{name}");
        }
        for unknown in [
            "hevc_mf",
            "h264_vulkan",
            "av1_d3d12va",
            "libx264",
            "hevc",
            "",
        ] {
            assert_eq!(
                B::from_ffmpeg_name(unknown),
                None,
                "{unknown:?} must not parse"
            );
        }
    }

    /// The additive-list rule for cells: a newer agent's unknown codec /
    /// backend / chroma is skipped, never an error, and a pre-FR-77 agent
    /// (no `video_cells` key at all) yields no cells and `has_cell == false`.
    #[test]
    fn unknown_cells_are_ignored_and_absent_cells_advertise_nothing() {
        let caps = AgentCaps {
            video_cells: vec![
                VideoCell::new(
                    VideoCodec::Hevc,
                    VideoBackend::Nvenc,
                    &[ChromaFormat::Yuv420, ChromaFormat::Yuv444],
                    true,
                ),
                VideoCell {
                    codec: "vvc".into(),
                    backend: "nvenc".into(),
                    chroma: vec!["yuv420".into()],
                    hw: true,
                },
                VideoCell {
                    codec: "av1".into(),
                    backend: "vulkan".into(),
                    chroma: vec!["yuv420".into()],
                    hw: true,
                },
                VideoCell {
                    codec: "vp9".into(),
                    backend: "libvpx".into(),
                    chroma: vec!["yuv420".into(), "yuv422".into(), "yuv444".into()],
                    hw: false,
                },
            ],
            ..Default::default()
        };
        let typed = caps.typed_cells();
        assert_eq!(
            typed.len(),
            2,
            "the vvc and vulkan cells are unknown here: {typed:?}"
        );
        assert_eq!(
            typed[1].chroma,
            vec![ChromaFormat::Yuv420, ChromaFormat::Yuv444]
        );
        assert!(caps.has_cell(VideoCodec::Hevc, ChromaFormat::Yuv444));
        assert!(
            !caps.has_cell(VideoCodec::Av1, ChromaFormat::Yuv420),
            "the vulkan cell is unreadable here"
        );
        assert!(!caps.has_cell(VideoCodec::H264, ChromaFormat::Yuv420));

        // The pre-FR-77 shape: the six bare fields only.
        let old: AgentCaps = serde_json::from_str(
            r#"{"hw_encoders":["ffmpeg-hevc_nvenc"],"codecs":["h264","h265"],"has_input_permission":true,"supports_clipboard":true,"supports_file_transfer":true,"max_simultaneous_sessions":2}"#,
        )
        .unwrap();
        assert!(old.video_cells.is_empty());
        assert!(old.probe_ms.is_none());
        assert!(!old.has_cell(VideoCodec::Hevc, ChromaFormat::Yuv420));

        // Empty cells and an absent probe time stay OFF the wire.
        let wire = serde_json::to_string(&AgentCaps::default()).unwrap();
        assert!(
            !wire.contains("video_cells") && !wire.contains("probe_ms"),
            "{wire}"
        );

        // And a full round trip keeps every field of a cell.
        let wire = serde_json::to_string(&caps).unwrap();
        let back: AgentCaps = serde_json::from_str(&wire).unwrap();
        assert_eq!(back.video_cells, caps.video_cells);
    }

    #[test]
    fn exec_policy_defaults_are_closed() {
        // The whole safety story rests on this: a device row that predates
        // Fleet RPC — i.e. every device in the fleet today — must deserialise
        // to a policy that refuses.
        let p: ExecPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(p.mode, ExecMode::Off);
        assert!(!p.can_originate);
        assert_eq!(p.effective_consent_mode(), ConsentMode::Prompt);

        // …and so must an Agent row with no `exec_policy` key at all.
        let caps_free = r#"{
            "tenant_id":"000000000000000000000001",
            "owner_user_id":"000000000000000000000002",
            "name":"old","machine_id":"m","os":"windows",
            "agent_version":"0.1.0","agent_token_hash":"",
            "status":"offline","last_seen_at":{"$date":{"$numberLong":"0"}},
            "created_at":{"$date":{"$numberLong":"0"}},
            "updated_at":{"$date":{"$numberLong":"0"}},
            "deleted_at":null
        }"#;
        let a: Agent = serde_json::from_str(caps_free).unwrap();
        assert_eq!(a.exec_policy.mode, ExecMode::Off);
        // The SSH policy rides the same row and carries the same guarantee:
        // shipping the feature must not retroactively open a single device.
        assert_eq!(a.ssh_policy.mode, SshMode::Off);
    }

    #[test]
    fn ssh_policy_defaults_are_closed() {
        let p: SshPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(p.mode, SshMode::Off);
        assert!(!p.can_originate);
        // The consent default (unset ⇒ Prompt) is decided where the grant is
        // minted — `agent_ssh::effective_wire_consent`, locked by the api
        // crate's `only_an_explicit_auto_crosses_the_wire_as_auto`. Here the
        // raw field just deserialises to "the admin said nothing".
        assert_eq!(p.consent_mode, None);
        // `Daemon` is root-equivalent, so it must be a DELIBERATE choice that
        // an operator can see — not something a device silently acquires. It
        // is the default only because it is the one mode P2 can actually
        // perform; P5 is what makes the others real.
        assert_eq!(p.account_mode, SshAccountMode::Daemon);
    }

    #[test]
    fn ssh_policy_allowlists_restrict_only_when_populated() {
        let user = ObjectId::new();
        let role = ObjectId::new();
        let other = ObjectId::new();

        let gates = |p: SshPolicy| p.split().0;

        // Empty lists mean "no restriction at THIS layer" — never "deny all".
        // The permission bit and the org kill-switch are the layers that decide
        // whether anyone may connect at all.
        let open = gates(SshPolicy::default());
        assert!(open.allows_caller(&user, &[role]));

        let by_user = gates(SshPolicy {
            allowed_user_ids: vec![user],
            ..Default::default()
        });
        assert!(by_user.allows_caller(&user, &[]));
        assert!(!by_user.allows_caller(&other, &[]));

        let by_role = gates(SshPolicy {
            allowed_role_ids: vec![role],
            ..Default::default()
        });
        assert!(by_role.allows_caller(&other, &[role]));
        assert!(!by_role.allows_caller(&other, &[]));

        // Both populated ⇒ both must match; they are AND, not OR, so
        // narrowing by role cannot widen who the user allowlist admitted.
        let both = gates(SshPolicy {
            allowed_user_ids: vec![user],
            allowed_role_ids: vec![role],
            ..Default::default()
        });
        assert!(both.allows_caller(&user, &[role]));
        assert!(!both.allows_caller(&user, &[other]));
        assert!(!both.allows_caller(&other, &[role]));
    }

    #[test]
    fn ssh_policy_split_routes_every_field() {
        // The split is the ignored-field firewall: every policy field must come
        // out in exactly one half, unchanged. If this test needs editing, make
        // sure the field you added is actually ACTED ON by its consumer — the
        // whole point of `split` is that routing a field is a decision, not a
        // formality.
        let user = ObjectId::new();
        let role = ObjectId::new();
        let policy = SshPolicy {
            mode: SshMode::On,
            can_originate: true,
            allowed_user_ids: vec![user],
            allowed_role_ids: vec![role],
            account_mode: SshAccountMode::ConsoleUser,
            account: Some("goran".into()),
            consent_mode: Some(ConsentMode::Prompt),
        };
        let (gates, spec) = policy.split();
        assert_eq!(gates.mode, SshMode::On);
        assert!(gates.can_originate);
        assert_eq!(gates.allowed_user_ids, vec![user]);
        assert_eq!(gates.allowed_role_ids, vec![role]);
        assert_eq!(spec.account_mode, SshAccountMode::ConsoleUser);
        assert_eq!(spec.account.as_deref(), Some("goran"));
        assert_eq!(spec.consent_mode, Some(ConsentMode::Prompt));
    }

    #[test]
    fn exec_policy_consent_collapses_session_shaped_modes() {
        // Email / Push / PromptThenEmail are approve-link flows with no exec
        // equivalent. They must collapse to Prompt, never silently to Auto.
        for m in [
            ConsentMode::Email,
            ConsentMode::Push,
            ConsentMode::PromptThenEmail,
        ] {
            let p = ExecPolicy {
                consent_mode: Some(m),
                ..Default::default()
            };
            assert_eq!(p.effective_consent_mode(), ConsentMode::Prompt, "{m:?}");
        }
        let p = ExecPolicy {
            consent_mode: Some(ConsentMode::Auto),
            ..Default::default()
        };
        assert_eq!(p.effective_consent_mode(), ConsentMode::Auto);
    }

    #[test]
    fn empty_shell_resolves_before_the_allowlist_is_checked() {
        // Field-caught: `roomler exec <dev> -- …` and every `roomler diag`
        // bundle send an EMPTY shell meaning "the host default". Comparing
        // that literal "" against ["powershell","pwsh","cmd"] refused them
        // all, which made diag unusable on every device that had narrowed its
        // allowlist — i.e. every device an admin had actually configured.
        let narrowed = ExecPolicy {
            shells: vec!["powershell".into(), "pwsh".into(), "cmd".into()],
            ..Default::default()
        };
        assert!(
            !narrowed.allows_shell(""),
            "the raw empty string must NOT match — that is why resolution has to happen first"
        );
        assert!(narrowed.allows_shell(&ExecPolicy::resolve_shell("", OsKind::Windows)));
        assert!(narrowed.allows_shell(&ExecPolicy::resolve_shell("auto", OsKind::Windows)));
        assert!(narrowed.allows_shell(&ExecPolicy::resolve_shell("  ", OsKind::Windows)));

        // …and a genuinely disallowed shell is still refused after resolution.
        assert!(!narrowed.allows_shell(&ExecPolicy::resolve_shell("bash", OsKind::Windows)));
    }

    #[test]
    fn resolve_shell_matches_the_agents_own_defaults() {
        // These MUST equal the agent's `exec::resolve_shell` mapping, or the
        // policy check and the execution would disagree about what ran.
        assert_eq!(ExecPolicy::resolve_shell("", OsKind::Windows), "powershell");
        assert_eq!(ExecPolicy::resolve_shell("", OsKind::Linux), "bash");
        assert_eq!(ExecPolicy::resolve_shell("", OsKind::Macos), "bash");
        // An explicit request passes through untouched (trimmed).
        assert_eq!(ExecPolicy::resolve_shell(" pwsh ", OsKind::Windows), "pwsh");
        assert_eq!(ExecPolicy::resolve_shell("sh", OsKind::Linux), "sh");
    }

    #[test]
    fn exec_policy_shell_allowlist() {
        let open = ExecPolicy::default();
        assert!(open.allows_shell("pwsh"), "empty list allows everything");

        let narrowed = ExecPolicy {
            shells: vec!["pwsh".into()],
            ..Default::default()
        };
        assert!(narrowed.allows_shell("pwsh"));
        assert!(
            narrowed.allows_shell("PWSH"),
            "shell names are ASCII-insensitive"
        );
        assert!(!narrowed.allows_shell("cmd"));
    }

    #[test]
    fn exec_limits_clamp() {
        use exec_limits::*;
        // 0 means "unspecified" on the wire, not "instant timeout".
        assert_eq!(clamp_timeout_ms(0), DEFAULT_TIMEOUT_MS);
        assert_eq!(clamp_timeout_ms(1_000), 1_000);
        assert_eq!(clamp_timeout_ms(u64::MAX), MAX_TIMEOUT_MS);

        assert_eq!(clamp_output_bytes(0), DEFAULT_MAX_OUTPUT_BYTES);
        assert_eq!(clamp_output_bytes(1024), 1024);
        assert_eq!(clamp_output_bytes(u64::MAX), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn overlay_ip_adds_host_to_cgnat_base() {
        let cidr = OverlayNetwork::DEFAULT_CIDR;
        assert_eq!(overlay_ip(cidr, 1).as_deref(), Some("100.64.0.1"));
        assert_eq!(overlay_ip(cidr, 7).as_deref(), Some("100.64.0.7"));
        assert_eq!(overlay_ip(cidr, 256).as_deref(), Some("100.64.1.0"));
        assert_eq!(overlay_ip("not-a-cidr", 1), None);
    }

    #[test]
    fn cidr_bounds_cap_host_ordinals_to_the_block() {
        // Multi-org P2a — the whole reason blocks can't bleed: /22 = 1024
        // addresses ⇒ hosts 1..=1022 leaseable.
        assert_eq!(cidr_max_host("100.68.12.0/22"), Some(1022));
        assert_eq!(cidr_max_host("100.64.0.0/10"), Some((1 << 22) - 2));
        assert_eq!(cidr_max_host("100.64.0.0/30"), Some(2));
        assert_eq!(cidr_max_host("100.64.0.0/31"), None, "no leasable hosts");
        assert_eq!(cidr_max_host("100.64.0.0/32"), None);
        assert_eq!(cidr_max_host("garbage"), None);

        // overlay_ip refuses to render past the block…
        assert_eq!(
            overlay_ip("100.68.12.0/22", 1022).as_deref(),
            Some("100.68.15.254")
        );
        assert_eq!(
            overlay_ip("100.68.12.0/22", 1023),
            None,
            "block's last address"
        );
        assert_eq!(overlay_ip("100.68.12.0/22", 1024), None, "neighbor block");
        // …and overlay_host rejects an address above it (a row leased under a
        // since-shrunk CIDR must not poison the pool).
        assert_eq!(overlay_host("100.68.12.0/22", "100.68.15.254"), Some(1022));
        assert_eq!(overlay_host("100.68.12.0/22", "100.68.16.1"), None);
        // The below-base rejection is unchanged.
        assert_eq!(overlay_host("100.68.12.0/22", "100.68.11.9"), None);
    }

    #[test]
    fn overlay_host_inverts_overlay_ip() {
        let cidr = OverlayNetwork::DEFAULT_CIDR;
        assert_eq!(overlay_host(cidr, "100.64.0.7"), Some(7));
        assert_eq!(overlay_host(cidr, "100.64.1.0"), Some(256));
        assert_eq!(overlay_host("10.0.0.0/8", "10.0.0.1"), Some(1));
    }

    /// The contract that keeps the free pool honest: whatever `overlay_ip`
    /// hands out, `overlay_host` must recover exactly. If someone later masks
    /// `overlay_ip` to the network address without fixing the inverse, released
    /// host numbers would be off and the pool would hand out wrong addresses.
    #[test]
    fn overlay_host_round_trips_for_every_host() {
        let cidr = OverlayNetwork::DEFAULT_CIDR;
        for host in [1u32, 2, 255, 256, 65_535, 1_000_000, 4_194_302] {
            let ip = overlay_ip(cidr, host).expect("forward");
            assert_eq!(overlay_host(cidr, &ip), Some(host), "round-trip for {host}");
        }
    }

    /// Both directions ignore the prefix identically, so a non-canonical CIDR
    /// (base not masked to the prefix) still round-trips.
    #[test]
    fn overlay_host_ignores_the_prefix_like_overlay_ip_does() {
        let cidr = "100.64.0.5/10";
        assert_eq!(overlay_ip(cidr, 2).as_deref(), Some("100.64.0.7"));
        assert_eq!(overlay_host(cidr, "100.64.0.7"), Some(2));
    }

    #[test]
    fn overlay_host_rejects_out_of_range_and_malformed() {
        let cidr = OverlayNetwork::DEFAULT_CIDR;
        // Below the base — a lease from a different, since-changed CIDR.
        assert_eq!(overlay_host(cidr, "10.0.0.1"), None);
        assert_eq!(overlay_host("not-a-cidr", "1.2.3.4"), None);
        assert_eq!(overlay_host(cidr, "nope"), None);
    }

    // --- P2b tenant blocks ------------------------------------------------

    #[test]
    fn block_cidr_renders_the_slot_grid() {
        // The first allocatable slot is 100.65.0.0 — everything below it is
        // the legacy reserve.
        assert_eq!(
            block_cidr_for_slot(OVERLAY_BLOCK_FIRST_SLOT, 1).as_deref(),
            Some("100.65.0.0/22")
        );
        assert_eq!(block_cidr_for_slot(65, 1).as_deref(), Some("100.65.4.0/22"));
        assert_eq!(
            block_cidr_for_slot(128, 1).as_deref(),
            Some("100.66.0.0/22")
        );
        // Wider blocks widen the prefix.
        assert_eq!(block_cidr_for_slot(64, 4).as_deref(), Some("100.65.0.0/20"));
        assert_eq!(
            block_cidr_for_slot(64, 64).as_deref(),
            Some("100.65.0.0/16")
        );
        // Unaligned / non-power-of-two / past the /10 are refused.
        assert_eq!(block_cidr_for_slot(65, 4), None, "unaligned");
        assert_eq!(block_cidr_for_slot(64, 3), None, "not a power of two");
        assert_eq!(block_cidr_for_slot(OVERLAY_BLOCK_SLOT_COUNT, 1), None);
        assert_eq!(block_cidr_for_slot(OVERLAY_BLOCK_SLOT_COUNT - 1, 2), None);
        // The last slot IS allocatable.
        assert_eq!(
            block_cidr_for_slot(OVERLAY_BLOCK_SLOT_COUNT - 1, 1).as_deref(),
            Some("100.127.252.0/22")
        );
    }

    #[test]
    fn block_prefix_maps_to_slot_width() {
        assert_eq!(block_slots_for_prefix(22), Some(1));
        assert_eq!(block_slots_for_prefix(20), Some(4));
        assert_eq!(block_slots_for_prefix(16), Some(64));
        assert_eq!(block_slots_for_prefix(23), None, "sub-slot");
        assert_eq!(block_slots_for_prefix(15), None, "wider than /16");
        assert_eq!(block_slots_for_prefix(0), None);
    }

    /// The allocator's safety property: starts are aligned and monotonic, so
    /// two racers computing from the same "highest end" either collide on the
    /// SAME slot (the unique index arbitrates) or claim disjoint ranges. This
    /// walks every width against every possible predecessor end and asserts
    /// the ranges never partially overlap.
    #[test]
    fn aligned_starts_are_never_partially_overlapping() {
        for after in 0u32..512 {
            let mut claims: Vec<(u32, u32)> = Vec::new();
            for prefix in OVERLAY_BLOCK_MIN_PREFIX..=OVERLAY_BLOCK_MAX_PREFIX {
                let slots = block_slots_for_prefix(prefix).expect("supported prefix");
                let start = block_align_slot(after, slots);
                assert!(start >= after, "monotonic: {start} >= {after}");
                assert_eq!(start % slots, 0, "aligned");
                assert!(
                    start - after < slots,
                    "no more than one width of waste (start {start}, after {after})"
                );
                claims.push((start, slots));
            }
            // Buddy property: any two claims are identical-start or disjoint.
            for (i, (s1, w1)) in claims.iter().enumerate() {
                for (s2, w2) in claims.iter().skip(i + 1) {
                    let overlap = s1.max(s2) < &(s1 + w1).min(s2 + w2);
                    assert!(
                        !overlap || s1 == s2,
                        "partial overlap: [{s1},{}) vs [{s2},{})",
                        s1 + w1,
                        s2 + w2
                    );
                }
            }
        }
    }

    /// A carved block's leasable range must stay strictly inside it — the
    /// whole point of the P2a ceiling.
    #[test]
    fn block_leases_stay_inside_the_block() {
        let cidr = block_cidr_for_slot(64, 1).expect("slot 64");
        assert_eq!(cidr, "100.65.0.0/22");
        assert_eq!(cidr_max_host(&cidr), Some(1022));
        assert_eq!(overlay_ip(&cidr, 1).as_deref(), Some("100.65.0.1"));
        assert_eq!(overlay_ip(&cidr, 1022).as_deref(), Some("100.65.3.254"));
        // …and the first address of the NEXT block is unreachable from here.
        assert_eq!(overlay_ip(&cidr, 1024), None);
        assert_eq!(overlay_host(&cidr, "100.65.4.0"), None);
    }

    /// `DesiredConfig` is an ALLOWLIST, and two keys are outside it because a
    /// server able to set them could open everything else: the device's opt-in
    /// (`remote_config_enabled`) and the device's SSH privilege ceiling
    /// (`ssh_max_privilege`, M5). Both are commented at the type — this is the
    /// part that fails a build when someone adds them "for symmetry".
    #[test]
    fn the_device_owned_refusals_are_not_pushable() {
        // Every field populated, so nothing is skipped by
        // `skip_serializing_if` and the key set is the whole surface.
        let full = DesiredConfig {
            exec_enabled: Some(true),
            ssh_enabled: Some(true),
            ssh_authorized_keys: Some(vec!["ssh-ed25519 AAAA".into()]),
            ssh_account_mode: Some("daemon".into()),
            ssh_port: Some(2222),
            revision: 1,
            updated_by: Some(ObjectId::new()),
            updated_at: Some(DateTime::now()),
        };
        let json = serde_json::to_value(&full).expect("serialises");
        let keys: Vec<&str> = json
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        for forbidden in ["remote_config_enabled", "ssh_max_privilege"] {
            assert!(
                !keys.contains(&forbidden),
                "{forbidden} must never be pushable by the server — it is the device's own \
                 refusal, and a server that can set it can set everything. Got keys: {keys:?}"
            );
        }

        // And the receiving side ignores it rather than erroring, which is the
        // safe direction: a server asserting it gets no effect, not a refused
        // config push that would also drop the legitimate keys beside it.
        let pushed: DesiredConfig = serde_json::from_value(serde_json::json!({
            "ssh_enabled": true,
            "ssh_max_privilege": "daemon",
            "remote_config_enabled": true,
        }))
        .expect("unknown keys are ignored, not fatal");
        assert_eq!(pushed.ssh_enabled, Some(true));
    }
}

#[cfg(test)]
mod block_list_tests {
    use super::*;

    /// THE compatibility property: a one-block list is byte-for-byte the
    /// behaviour of the bare `overlay_ip`/`overlay_host` pair. Multi-block
    /// ships behind a flag on a live fleet, so "off" has to be provably the
    /// old code path rather than a re-implementation that agrees most of the
    /// time.
    #[test]
    fn a_single_block_list_is_exactly_the_old_pair() {
        let cidr = "100.65.4.0/22";
        let bl = BlockList::single(cidr);
        assert_eq!(bl.capacity(), cidr_max_host(cidr).unwrap());
        for h in [1u32, 2, 7, 255, 256, 1021, 1022] {
            let want = overlay_ip(cidr, h).unwrap();
            assert_eq!(bl.ip_for_ordinal(h).as_deref(), Some(want.as_str()));
            assert_eq!(bl.ordinal_for_ip(&want), Some(h));
        }
        // Past the end in both directions.
        assert_eq!(bl.ip_for_ordinal(1023), None);
        assert_eq!(bl.ip_for_ordinal(0), None);
        assert_eq!(bl.ordinal_for_ip("100.66.0.1"), None);
    }

    /// Ordinals concatenate across blocks, and the round-trip holds over the
    /// seam — which is the only place it can plausibly break.
    #[test]
    fn ordinals_concatenate_across_blocks_and_round_trip() {
        let bl = BlockList::new(["100.65.4.0/22", "100.65.8.0/22", "100.66.0.0/20"]);
        assert_eq!(bl.capacity(), 1022 + 1022 + 4094);

        // Last of block 0, first of block 1 — the seam.
        assert_eq!(bl.ip_for_ordinal(1022).as_deref(), Some("100.65.7.254"));
        assert_eq!(bl.ip_for_ordinal(1023).as_deref(), Some("100.65.8.1"));
        // Last of block 1, first of block 2 — the second seam.
        assert_eq!(bl.ip_for_ordinal(2044).as_deref(), Some("100.65.11.254"));
        assert_eq!(bl.ip_for_ordinal(2045).as_deref(), Some("100.66.0.1"));

        for o in [1u32, 1022, 1023, 2044, 2045, 6138] {
            let ip = bl.ip_for_ordinal(o).expect("in range");
            assert_eq!(bl.ordinal_for_ip(&ip), Some(o), "round trip at ordinal {o}");
        }
        assert_eq!(bl.ip_for_ordinal(bl.capacity() + 1), None);
    }

    /// Appending a block must not move a single existing ordinal. This is the
    /// property that makes growth non-disruptive, and it is the whole reason
    /// the list is append-only.
    #[test]
    fn appending_a_block_moves_no_existing_ordinal() {
        let before = BlockList::new(["100.65.4.0/22"]);
        let after = BlockList::new(["100.65.4.0/22", "100.65.8.0/22"]);
        for o in 1..=before.capacity() {
            assert_eq!(
                before.ip_for_ordinal(o),
                after.ip_for_ordinal(o),
                "ordinal {o} moved when a block was appended"
            );
        }
        assert!(before.ip_for_ordinal(1023).is_none());
        assert!(after.ip_for_ordinal(1023).is_some());
    }

    /// A node is told the block its OWN address lives in — an agent derives
    /// its TUN netmask and NAT scope from that string, and the first block
    /// would be wrong for a node living in the second.
    #[test]
    fn cidr_for_ip_names_the_block_that_contains_the_address() {
        let bl = BlockList::new(["100.65.4.0/22", "100.65.8.0/22"]);
        assert_eq!(bl.cidr_for_ip("100.65.4.2"), Some("100.65.4.0/22"));
        assert_eq!(bl.cidr_for_ip("100.65.8.2"), Some("100.65.8.0/22"));
        assert_eq!(bl.cidr_for_ip("100.70.0.1"), None);
    }

    /// A block that can lease nothing is dropped, so it can never become a
    /// position that later shifts every ordinal above it.
    #[test]
    fn unleasable_blocks_are_dropped_not_kept_as_zero_width() {
        let bl = BlockList::new(["100.65.4.0/22", "100.65.8.0/31", "100.65.12.0/22"]);
        assert_eq!(bl.cidrs().len(), 2);
        assert_eq!(bl.capacity(), 2044);
        assert_eq!(bl.ip_for_ordinal(1023).as_deref(), Some("100.65.12.1"));
    }
}

// FR-69 P5a — the SSH policy REQUEST shape, here because the fleet module
// accepts it inside its agent-update body while the SSH route (network) reads
// and writes it: a wire shape both sides share lives in the wire crate.
#[derive(Debug, Serialize, Deserialize)]
pub struct SshPolicyBody {
    #[serde(default)]
    pub mode: SshMode,
    #[serde(default)]
    pub can_originate: bool,
    #[serde(default)]
    pub allowed_user_ids: Vec<String>,
    #[serde(default)]
    pub allowed_role_ids: Vec<String>,
    #[serde(default)]
    pub account_mode: SshAccountMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_mode: Option<ConsentMode>,
}

impl From<SshPolicy> for SshPolicyBody {
    /// The stored policy as the dashboard reads it back — hex ids, because
    /// bson's `ObjectId` serialises to `{"$oid": …}` and no client here parses
    /// that.
    ///
    /// Destructured EXHAUSTIVELY for the same reason [`SshPolicy::split`] is:
    /// this is the shape a dialog re-saves, so a field added to [`SshPolicy`]
    /// and forgotten here would not merely fail to display — the next save
    /// would reset it to its default. Better a compile error.
    fn from(p: SshPolicy) -> Self {
        let SshPolicy {
            mode,
            can_originate,
            allowed_user_ids,
            allowed_role_ids,
            account_mode,
            account,
            consent_mode,
        } = p;
        Self {
            mode,
            can_originate,
            allowed_user_ids: allowed_user_ids.iter().map(|i| i.to_hex()).collect(),
            allowed_role_ids: allowed_role_ids.iter().map(|i| i.to_hex()).collect(),
            account_mode,
            account,
            consent_mode,
        }
    }
}

// FR-69 P5c — the key-rotation order predicates the agent socket evaluates on
// connect. Here (the wire crate) because the socket is the fleet module`s and
// the order is network`s: a pure judgement over two models belongs with them.
/// P1b — how long a DELIVERED order is trusted to be in progress before the
/// connect-time reconcile pushes it again. Found in the first field run: the
/// device's `rotated` report rides the dying session and is written by a
/// spawned task, while the device reconnects ~500 ms later — its register ran
/// the reconcile before the report landed, re-pushed the SAME order, the
/// device refused the duplicate under its own 60 s ceiling, and that refusal
/// overwrote the `rotated` report. A freshly delivered order is being
/// executed; its answer is seconds away. Past this window an unanswered order
/// is assumed dropped (the device crashed mid-rotation, say) and re-sent.
pub const REDELIVER_AFTER_SECS: i64 = 120;

/// P1d — an order is SATISFIED once the device has joined under a key that
/// differs from the one it held when the order was placed, whether or not a
/// report ever arrived. Found in the third cycle: the run-2 order's report was
/// lost, the P1b window expired, and every later reconnect (a pod roll, then
/// the 0.4.26 restart) re-delivered the same order — the device rotated three
/// times for one click. The join is the proof; a satisfied order is never
/// pushed again. Orders placed before the snapshot existed (no
/// `public_key_before`) cannot be judged this way and fall back to the report.
pub fn order_is_satisfied(
    request: &KeyRotationRequest,
    identity: Option<&OverlayIdentity>,
) -> bool {
    match (request.public_key_before.as_deref(), identity) {
        (Some(before), Some(id)) => {
            id.public_key != before
                && id.joined_at.timestamp_millis() >= request.requested_at.timestamp_millis()
        }
        _ => false,
    }
}

/// Whether a standing order should be pushed again on THIS connect (the
/// report-and-timing half; callers also check [`order_is_satisfied`]).
pub fn should_redeliver(
    request: &KeyRotationRequest,
    report: Option<&KeyRotationReport>,
    now: DateTime,
) -> bool {
    if report.is_some_and(|r| r.request_id == request.request_id) {
        return false;
    }
    match request.delivered_at {
        None => true,
        Some(at) => (now.timestamp_millis() - at.timestamp_millis()) / 1000 >= REDELIVER_AFTER_SECS,
    }
}
