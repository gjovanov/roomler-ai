// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Codec capability detection.
//!
//! Probes which video codecs the local host can encode and reports
//! them in the agent's `rc:agent.hello` payload. The result populates
//! `AgentCaps.codecs` (mime-style names like `"h264"`, `"h265"`,
//! `"av1"`) and `AgentCaps.hw_encoders` (descriptive labels like
//! `"mf-h264-hw"`, `"openh264-sw"`).
//!
//! Used by Phase 2 codec negotiation: the controller's browser
//! advertises its `RTCRtpReceiver.getCapabilities('video').codecs`
//! and the agent picks the best intersection.
//!
//! Detection is **probe-gated** for codecs without a safe demotion
//! path (HEVC, AV1): we actually run a tiny MfEncoder::new at startup
//! and only advertise codecs that successfully activate. This closes
//! the "enumerates but won't activate" false-advertising gap (e.g.
//! NVIDIA RTX 5090 Blackwell where the AV1 MFT enumerates but every
//! `ActivateObject` returns 0x8000FFFF). Without this guard a browser
//! session could negotiate AV1, the pump's runtime cascade would fail,
//! and the fail-closed NoopEncoder would leave the browser with a
//! black screen. The probe result is cached behind a `OnceLock` so
//! the ~300ms / codec init cost runs once per agent process, not per
//! `rc:agent.hello`.

use roomler_ai_remote_control::models::AgentCaps;
// FR-77 — the cell vocabulary. Which of these a build uses depends on its
// encoder features, so the import is allowed to be partly idle.
#[allow(unused_imports)]
use roomler_ai_remote_control::models::{ChromaFormat, VideoBackend, VideoCell, VideoCodec};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

#[allow(unused_imports)]
use super::cells::{cell_denied, denied_cells, ffmpeg_444_capable};

static CACHED_CAPS: OnceLock<AgentCaps> = OnceLock::new();
/// #3280 — a later try's answer, once one is adopted ([`adopt`]). [`CACHED_CAPS`]
/// keeps the first and keeps serialising the first probe, so every reader asks
/// here first.
static LATE_CAPS: RwLock<Option<AgentCaps>> = RwLock::new(None);
/// #3280 — moves when the answer in force does. Each connection's heartbeat
/// compares it, so every organization's row gets the new answer within a beat.
static CAPS_GENERATION: AtomicU64 = AtomicU64::new(0);
/// #3280 — what the first answer ran out of time on. [`arm_retry`] takes it, so a
/// second organization's loop finds nothing to arm.
static FIRST_SHORT: Mutex<Option<child::Shortfall>> = Mutex::new(None);
/// #3280 — when a session last asked for this host ([`session_asked`]).
static LAST_ASK: Mutex<Option<Instant>> = Mutex::new(None);
/// #3280 — the GPU-input forms of the answer in force, for [`weigh`].
static GPU_IN_FORCE: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn late_caps() -> Option<AgentCaps> {
    LATE_CAPS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// FR-84 D4 — the probe's result if it has run in this process, else `None`.
///
/// Reads what is already there and nothing else — deliberately NOT `detect()`. The
/// LocalAPI serves this on every Overview poll, and the first `detect()` in a
/// process spawns the probe children (vendor driver code, the reason the
/// probe is out-of-process at all); a control-surface read must never be what
/// launches that. The probe runs at the first `rc:agent.hello`, so before a
/// daemon has reached its server this is `None`, and the Overview says
/// "not probed yet" and asks again later.
pub fn cached() -> Option<AgentCaps> {
    late_caps().or_else(|| CACHED_CAPS.get().cloned())
}

/// #3280 — a number that changes when the answer [`detect`] gives does. Read it
/// BEFORE reading the answer: a change in between then reads as news next time.
pub fn generation() -> u64 {
    CAPS_GENERATION.load(Ordering::SeqCst)
}

/// Whether this build carries ANY video encoder. A signalling-only build
/// (`cargo build -p roomlerd` with no media feature) has nothing to probe and
/// nothing to advertise, so the Overview says `unsupported` instead of
/// waiting for a probe that will never produce a cell.
pub const ENCODERS_COMPILED: bool = cfg!(any(
    feature = "openh264-encoder",
    feature = "mf-encoder",
    feature = "ffmpeg-encoder",
    feature = "vp9-444"
));

/// FR-84 D4 — the Overview's view of a caps value: the cell matrix, the
/// legacy labels, the probe timing, the denylist and the resolved preference.
/// Pure — `caps` is whatever the caller holds (the cache, a fixture).
pub fn summarize(
    caps: Option<&AgentCaps>,
    denied: Vec<String>,
    preference: Option<&str>,
) -> tunnel_core::localapi::EncoderCapsSummary {
    use tunnel_core::localapi::{EncoderCapsSummary, EncoderCell};
    let state = if !ENCODERS_COMPILED {
        "unsupported"
    } else if caps.is_some() {
        "ready"
    } else {
        "not_probed"
    };
    let mut out = EncoderCapsSummary {
        state: state.to_string(),
        denied,
        encoder_preference: preference.map(str::to_string),
        ..Default::default()
    };
    if let Some(c) = caps {
        out.cells = c
            .video_cells
            .iter()
            .map(|cell| EncoderCell {
                codec: cell.codec.clone(),
                backend: cell.backend.clone(),
                chroma: cell.chroma.clone(),
                hardware: cell.hw,
            })
            .collect();
        out.hw_encoders = c.hw_encoders.clone();
        out.codecs = c.codecs.clone();
        out.probe_ms = c.probe_ms;
        out.probe_cached = c.probe_cached;
    }
    out
}

/// FR-84 D4 — what the LocalAPI `EncoderCaps` verb answers: the CACHED
/// probe (never a fresh one), the denylist in force (env > config > built-in,
/// plus what the probe learned, #3267) and the preference `run` resolved.
pub fn summarize_cached() -> tunnel_core::localapi::EncoderCapsSummary {
    summarize(
        cached().as_ref(),
        denied_cells(),
        super::resolved_preference().map(|p| p.wire()),
    )
}

/// Running the hardware probes in a CHILD PROCESS.
///
/// A capability probe is untrusted third-party code by definition — it calls
/// into vendor drivers and, through them, GPU firmware. In-process, a fault
/// anywhere in that stack takes the daemon down, and the service manager
/// restarts it straight back into the same probe: a crash-LOOP, not a
/// degraded-but-running agent.
///
/// Observed on the WSL sibling, 2026-08-20: WSL ships
/// `/usr/lib/wsl/lib/libcuda.so.1` as a stub with no usable driver, so
/// `hevc_nvenc` **dlopens successfully** and then segfaults when `cuInit(0)`
/// fails — the loaded-but-unusable state that no "is it present?" check
/// catches. Hosts with no libcuda at all take the clean dlopen-failed branch
/// and were never affected, which is why this stayed latent.
///
/// So the probes run behind a process boundary and **"the child died" is read
/// as "codec unavailable"**. That retires the whole class rather than the one
/// CUDA symptom: any future driver that faults, hangs, or calls `exit()` costs
/// this host at most its HW advertisement, and since #3267 only the cell it
/// was opening when it can name one ([`run_base`]).
mod child {
    use super::AgentCaps;
    use crate::encode::cells::{self, LearnedDeny};
    use roomler_ai_remote_control::models::{ChromaFormat, VideoBackend, VideoCodec};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Marks the line carrying the child's JSON, so log output on the same
    /// stream cannot be mistaken for the result. Parsing the last line, or
    /// all of stdout, would break the first time anything logged there.
    pub(super) const MARKER: &str = "ROOMLER_CAPS_JSON:";
    /// FR-77 P3c — the 4:4:4 phase's answer: a JSON array of the encoder
    /// names whose 4:4:4 form opened.
    pub(super) const MARKER_444: &str = "ROOMLER_CAPS_444:";
    /// FR-118 P2-3 — the GPU-input phase's answer: a JSON array of the encoder
    /// names whose GPU-input form opened and encoded a GPU frame.
    pub(super) const MARKER_GPU: &str = "ROOMLER_CAPS_GPU:";
    /// FR-77 P3c — printed by the child BEFORE each open it attempts. On a
    /// service host the child's stderr reaches no log, so this is the only
    /// record of which open a dying child was inside; the parent quotes the
    /// last one in its verdict.
    pub(super) const PROGRESS: &str = "ROOMLER_CAPS_PROGRESS:";
    /// #3267 — the value of a `PROGRESS` line that says the open before it
    /// ended, so a child that dies between opens (an enumeration, its own
    /// exit) is not read as dying in the last cell it opened.
    pub(super) const NO_OPEN: &str = "-";

    /// #3267 — one open in the `base` phase, announced for as long as this
    /// lives, so `last_open` names the cell if the process dies in it. Holds
    /// across the encoder's teardown too: radeonsi's fault was in the flush.
    /// Silent outside a probe child: nobody else reads these lines.
    pub(super) struct OpenWindow;

    impl OpenWindow {
        pub(super) fn begin(name: &str, chroma: ChromaFormat) -> Self {
            if super::in_probe_child() {
                emit(&format!("{PROGRESS}{name}:{}", chroma.wire()));
            }
            Self
        }
    }

    impl Drop for OpenWindow {
        fn drop(&mut self) {
            // A panic inside the open leaves the open announced: it died there.
            if super::in_probe_child() && !std::thread::panicking() {
                emit(&format!("{PROGRESS}{NO_OPEN}"));
            }
        }
    }

    /// #2719 — how a child whose parent stopped reading exits. In the field
    /// nobody is left to read it (the parent was terminated); a parent that
    /// is still there reads any non-zero exit as "no hardware", the safe
    /// answer.
    pub(super) const EXIT_PARENT_GONE: i32 = 3;

    /// #2719 — one line on stdout for the parent, and never a panic doing it.
    ///
    /// `println!` PANICS when the write fails, and on an update it does. The
    /// supervisor terminates the worker while its probe child still runs,
    /// the child's report then meets a pipe whose reader is gone (`os error
    /// 232` on Windows, EPIPE elsewhere), and the panic hook files that as
    /// an agent crash, on every update a stop meets a probe (CORPLAP-1,
    /// 0.4.151). The parent being gone is no crash: exit quietly. The flush
    /// is what keeps a PROGRESS line if the open it announces kills the
    /// process.
    pub(super) fn emit(line: &str) {
        match write_line(&mut std::io::stdout().lock(), line) {
            Ok(()) => {}
            Err(e) if parent_gone(&e) => std::process::exit(EXIT_PARENT_GONE),
            Err(e) => {
                tracing::error!(%e, "caps probe: could not write to stdout");
                std::process::exit(2);
            }
        }
    }

    pub(super) fn write_line(w: &mut impl std::io::Write, line: &str) -> std::io::Result<()> {
        writeln!(w, "{line}")?;
        w.flush()
    }

    /// Whether a failed write means the reading end is gone. std reads EPIPE
    /// and both Windows codes as `BrokenPipe`: `ERROR_BROKEN_PIPE` (109) and
    /// `ERROR_NO_DATA` (232, "the pipe is being closed"), the one the field
    /// saw (measured on 1.95). The numbers are matched as well, so a std that
    /// reads them otherwise cannot bring the crash back.
    pub(super) fn parent_gone(e: &std::io::Error) -> bool {
        e.kind() == std::io::ErrorKind::BrokenPipe
            || (cfg!(windows) && matches!(e.raw_os_error(), Some(109 | 232)))
    }

    /// Set in the child's environment. A belt-and-braces recursion guard:
    /// `detect()` in a process carrying this must never spawn again.
    pub(super) const CHILD_ENV: &str = "ROOMLERD_CAPS_CHILD";
    /// FR-77 P3c — which phase the child runs: `base` (every 4:2:0 open, no
    /// FFmpeg 4:4:4 attempt) or `444` (only the 4:4:4 forms named in
    /// [`NAMES_444_ENV`]). Unset = the pre-P3c single-phase probe.
    pub(super) const PHASE_ENV: &str = "ROOMLERD_CAPS_PHASE";
    pub(super) const NAMES_444_ENV: &str = "ROOMLERD_CAPS_444_NAMES";
    /// FR-118 P2-3 — the names the `gpu` phase tries.
    pub(super) const NAMES_GPU_ENV: &str = "ROOMLERD_CAPS_GPU_NAMES";

    /// Generous, because a cold GPU driver init on a loaded corp laptop is
    /// genuinely slow (~300 ms per codec, several codecs, plus process
    /// start). This is a backstop against a HUNG driver, not a performance
    /// budget — and even at the ceiling it beats the in-process behaviour it
    /// replaces, which was to hang forever.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    /// #3273 — how long a child that ended or was told to die, and then its
    /// stdout, get before [`run_child`] carries on without them. A healthy host
    /// waits none of this, as the pipe ends with the child. It is this long
    /// because a report is the last thing a child writes: a reader thread starved
    /// past this on a thrashing host costs that report, and with it the hardware
    /// until the next start. So it is long for a scheduler, and short for a
    /// device waiting to come online.
    const KILL_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

    /// What the child hands back: the caps, plus the per-host verdicts the
    /// probe measured that are NOT wire capabilities.
    ///
    /// The vp9_qsv runtime-IDR verdict (P4) used to be set in a `OnceLock`
    /// — in the CHILD, where nothing ever read it again. rc.433 moved the
    /// probe out of process and the verdict never crossed back: every
    /// vp9_qsv session since ran the GOP-60 containment the probe existed to
    /// lift. It rides this envelope now and the parent installs it
    /// (`install_vp9_qsv_idr`), and the cache keeps it with the caps.
    #[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
    pub(super) struct ProbeReport {
        pub(super) caps: AgentCaps,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub(super) vp9_qsv_idr: Option<(bool, bool)>,
        /// FR-118 P2-3 — the encoders whose GPU-input form the `gpu` phase
        /// proved: not a wire capability, the gate a session's GPU form reads
        /// (`gpu_input::plan`). Rides the envelope and the cache with the caps.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub(super) gpu_input: Vec<String>,
    }

    /// FR-77 P3c — which phase THIS process was started for.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Phase {
        /// The pre-P3c shape: everything in one process (a bare
        /// `roomlerd caps-probe`).
        Full,
        /// Every 4:2:0 open, the software cells, the IDR verdict — and NO
        /// FFmpeg 4:4:4 attempt.
        Base,
        /// Only the 4:4:4 forms of the names in [`NAMES_444_ENV`].
        Only444,
        /// FR-118 P2-3 — only the GPU-input forms of the names in
        /// [`NAMES_GPU_ENV`].
        OnlyGpu,
        /// #3267 — only the cells that need no driver ([`run_software`]).
        Software,
    }

    pub(super) fn phase_from_env() -> Phase {
        match std::env::var(PHASE_ENV).as_deref() {
            Ok("base") => Phase::Base,
            Ok("444") => Phase::Only444,
            Ok("gpu") => Phase::OnlyGpu,
            Ok("software") => Phase::Software,
            _ => Phase::Full,
        }
    }

    pub(super) fn names_444_from_env() -> Vec<String> {
        names_from_env(NAMES_444_ENV)
    }

    pub(super) fn names_gpu_from_env() -> Vec<String> {
        names_from_env(NAMES_GPU_ENV)
    }

    fn names_from_env(var: &str) -> Vec<String> {
        std::env::var(var)
            .map(|v| {
                v.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What one child run came back with. `stdout` is everything it printed
    /// (the progress lines survive a crash: Rust's stdout is line-buffered).
    pub(super) struct Run {
        pub(super) ending: Ending,
        pub(super) stdout: String,
        /// #3273 — the pipe had not ended at the bound, so `stdout` may be short
        /// of what the child wrote, its report included.
        pub(super) cut_off: bool,
        pub(super) elapsed: std::time::Duration,
    }

    /// How a child ended.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) enum Ending {
        /// Exit 0.
        Clean,
        /// It exited non-zero or was killed. `fault` = with a status a faulting
        /// driver leaves ([`is_fault`]); `status` is that status, as the log
        /// says it.
        Died { status: String, fault: bool },
        /// Still running at its bound ([`Bounds::child`]), and told to die for it.
        /// `gone` = it did (#3273): one that did not is stuck in a kernel call in
        /// a driver.
        Hung { gone: bool },
        /// #3280 — told to die because a session asked for this host
        /// ([`Try::stop`]). `gone` as for `Hung`.
        Stopped { gone: bool },
    }

    impl Ending {
        /// #3273 — it was told to die and had not within [`Bounds::kill_wait`].
        pub(super) fn stuck(&self) -> bool {
            matches!(
                self,
                Ending::Hung { gone: false } | Ending::Stopped { gone: false }
            )
        }
    }

    /// #3267 — whether a child's status is what a fault in a driver leaves,
    /// rather than someone ending the process or the host running out of
    /// something. An OOM kill and an EDR's `TerminateProcess` say nothing about
    /// the open they happened to land in, so they cannot earn a cached deny.
    pub(super) fn is_fault(status: &std::process::ExitStatus) -> bool {
        #[cfg(windows)]
        {
            status.code().is_some_and(|c| windows_fault_code(c as u32))
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal().is_some_and(unix_fault_signal)
        }
        #[cfg(not(any(windows, unix)))]
        {
            let _ = status;
            false
        }
    }

    /// #3267 — on Windows a process's exit code is the exception code that
    /// ended it. These are the ones a thread raises on itself by faulting.
    ///
    /// The list names what counts, and everything else does not. NTSTATUS has
    /// hundreds of error codes, and many are about the host rather than the
    /// open: out of resources (`0xC000009A`), a page that could not be read
    /// (`0xC0000006`), desktop heap (`0xC0000142`), an antivirus's verdict
    /// (`0xC0000906`). A code nobody has met yet must not be able to keep a
    /// healthy cell out for a day. Missing a real fault costs far less: the
    /// cell is denied for this process and tested again at the next start.
    ///
    /// 255 counts too, because the field says so: 2 of the 7 AMD deaths on the
    /// dev box (2026-11-01) read `exit code: 255`, and each lined up with an
    /// Application Error event for the same fault. Windows Error Reporting's
    /// own termination is the likely source; that part is unmeasured.
    #[cfg(windows)]
    fn windows_fault_code(code: u32) -> bool {
        const FAULTS: &[u32] = &[
            0xC000_0005, // access violation: the AMD fault
            0xC000_001D, // illegal instruction
            0xC000_0025, // a handler resumed what cannot be resumed
            0xC000_0094, // integer division by zero
            0xC000_0095, // integer overflow
            0xC000_0096, // privileged instruction
            0xC000_00FD, // stack overflow
            0xC000_0374, // heap corruption
            0xC000_0409, // fast fail: what `abort` leaves
            0xC000_041D, // a fault inside a user callback
            0xC000_0602, // fail fast
            0xE06D_7363, // an uncaught C++ exception
            255,         // the field's, above
        ];
        FAULTS.contains(&code)
    }

    /// #3267 — the signals a faulting driver raises on itself. Never SIGKILL
    /// (the OOM killer, an EDR, an operator) or SIGTERM (a stop).
    #[cfg(unix)]
    fn unix_fault_signal(signal: i32) -> bool {
        matches!(
            signal,
            libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE | libc::SIGABRT
        )
    }

    /// #3267 — `base` runs at most this many times per probe: four cells at
    /// two deaths each, and the run that reports. One whole vendor runtime
    /// that faults on every open is four cells (`vp9`, `hevc`, `av1`, `h264`).
    pub(super) const MAX_BASE_RUNS: u32 = 9;
    /// #3267 — no new `base` run starts this long after the first started. The
    /// base phase's worst case is therefore this plus one child's bound
    /// ([`TIMEOUT`] + [`KILL_WAIT`]): 160 s, against 60 s before, and it is only
    /// reached by a host whose children take a minute each. Long enough for one
    /// hang to get its retry, and for the three runs one fault costs on a host
    /// where a run takes under 45 s.
    /// The dev box's slowest run on record is 24 s.
    pub(super) const BASE_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(90);

    /// #3273 — the cells a session opens in the daemon whatever the caps say
    /// (`encode::open_default`), so a host that lost its hardware still runs their
    /// driver code in-process.
    pub(super) const MF_IN_THE_DAEMON: &[&str] = if cfg!(windows) {
        &[cells::MF_H264, cells::MF_HEVC, cells::MF_AV1]
    } else {
        &[]
    };

    /// #3273, #3280 — a child that would not die is a driver stuck in the kernel,
    /// and what the daemon opens whatever the caps say could hang the same way, on
    /// a wait with no bound. So those cells go too, for this process, whichever
    /// child it was. Not for a child whose kill landed within its bound: a merely
    /// slow host keeps them.
    fn media_foundation_denies() -> Vec<LearnedDeny> {
        MF_IN_THE_DAEMON
            .iter()
            .map(|name| LearnedDeny {
                cell: format!("{name}:{}", ChromaFormat::Yuv420.wire()),
                why: "a probe child would not die".to_string(),
                confirmed: false,
                timed_out: true,
            })
            .collect()
    }

    /// #3280 — a child stuck after the report left an answer offering what a session
    /// is now refused, and an RTP track binds to its codec before the encoder opens.
    /// Nothing here when the report was made without them.
    fn withdraw_media_foundation(caps: &mut AgentCaps) {
        caps.video_cells
            .retain(|c| c.backend != VideoBackend::MediaFoundation.wire());
        caps.hw_encoders.retain(|h| !h.starts_with("mf-"));
        let cells = &caps.video_cells;
        caps.codecs.retain(|codec| {
            let wire = if codec == "h265" {
                VideoCodec::Hevc.wire()
            } else {
                codec.as_str()
            };
            cells.iter().any(|c| c.codec == wire)
        });
    }

    /// [`media_foundation_denies`], onto a probe's own list.
    fn stuck_costs_media_foundation(denied: &mut Vec<LearnedDeny>) {
        for deny in media_foundation_denies() {
            if !denied.iter().any(|d| d.cell == deny.cell) {
                tracing::error!(
                    cell = %deny.cell,
                    "caps probe: a child would not die, so sessions stay out of this cell until \
                     a later try or the next start"
                );
                denied.push(deny);
            }
        }
    }

    /// What a death in `base` means for the next run.
    enum Step {
        RunAgain,
        GiveUp(&'static str),
        /// #3280 — as [`Self::GiveUp`], and for want of time.
        OutOfBudget,
    }

    /// #3267 — what the base runs have shown so far, and what earns a cell a
    /// place on the list. Pure apart from its log lines, so every OS tests it.
    ///
    /// - **A named cell that ends a child with a fault's status, twice in a
    ///   row, is learned and cached.** The first death only earns a second
    ///   run with nothing added: a transient fault passes it, and the cell
    ///   keeps its place in the matrix.
    /// - **A hang earns this process only.** A second hang would cost another
    ///   minute to confirm what load alone can produce (`docs/encoders.md` has
    ///   the one measured), so the run after it skips the cell, the result is
    ///   not cached, and a later try or the next start tests the cell again.
    /// - **Any other named death, twice, earns this process only** too: a kill
    ///   or an `exit()` may have been the cell's doing, and a session must not
    ///   be the next process to find out, but it proves too little to keep.
    /// - **A death with no cell to blame costs nothing but one more run.** A
    ///   second such death is the old answer: no hardware, and not cached.
    #[derive(Default)]
    struct BaseRetry {
        runs: u32,
        budget: std::time::Duration,
        denied: Vec<LearnedDeny>,
        /// The cell the last run died in, and whether that was a fault, until
        /// the next run says whether the cell was the cause.
        suspect: Option<(String, bool)>,
        blind_retry_used: bool,
    }

    impl BaseRetry {
        /// The child that ended with `ending` was inside `last_open`, and left
        /// no report. `elapsed` is since the first run started.
        fn weigh_death(
            &mut self,
            last_open: Option<&str>,
            ending: &Ending,
            elapsed: std::time::Duration,
        ) -> Step {
            let suspect = self.suspect.take();
            match last_open {
                // Nothing died and nothing was open: it just has no answer, and a
                // second run would say the same. A test binary, spawned as
                // `caps-probe`, ends this way in every integration test. So does a
                // report lost to the bound on the read (`Run::cut_off`, #3273): that
                // is out of time, so a later try asks again with more (#3280).
                None if *ending == Ending::Clean => {
                    return Step::GiveUp("a clean exit with no report");
                }
                None => {
                    if self.blind_retry_used {
                        return Step::GiveUp("two deaths with no cell to blame");
                    }
                    self.blind_retry_used = true;
                    tracing::warn!(
                        ?ending,
                        "caps probe: the base child died with no open in progress, so there is \
                         no cell to blame — running it once more"
                    );
                }
                Some(cell) if self.denied.iter().any(|d| d.cell == cell) => {
                    return Step::GiveUp("a child died in a cell it was told not to open");
                }
                Some(cell) if matches!(ending, Ending::Hung { .. }) => {
                    self.deny(cell, "hung".to_string(), false, true)
                }
                Some(cell) => {
                    let (status, fault) = match ending {
                        Ending::Died { status, fault } => (status.as_str(), *fault),
                        // A driver that called `exit(0)` in the open: a death.
                        _ => ("exited 0 with no report", false),
                    };
                    match suspect {
                        Some((again, was_fault)) if again == cell => {
                            self.deny(cell, status.to_string(), fault && was_fault, false)
                        }
                        _ => {
                            tracing::warn!(
                                cell,
                                status,
                                "caps probe: the base child died in this open — running it once \
                                 more with nothing added, to see whether the cell is the cause"
                            );
                            self.suspect = Some((cell.to_string(), fault));
                        }
                    }
                }
            }
            if self.runs >= MAX_BASE_RUNS {
                return Step::GiveUp("the run limit");
            }
            if elapsed >= self.budget {
                return Step::OutOfBudget;
            }
            Step::RunAgain
        }

        fn deny(&mut self, cell: &str, why: String, confirmed: bool, timed_out: bool) {
            if confirmed {
                tracing::error!(
                    cell,
                    why = %why,
                    "caps probe: this cell ended the base child twice, so it is denied: not \
                     opened by the probe or by a session, until the driver, this build or a \
                     ROOMLERD_* knob changes, or a restart a day from now"
                );
            } else {
                tracing::error!(
                    cell,
                    why = %why,
                    timed_out,
                    "caps probe: this cell is denied for this process only: a hang or a kill \
                     proves too little to keep, so the next start tests it again, and if it \
                     was a hang so does a later try"
                );
            }
            self.denied.push(LearnedDeny {
                cell: cell.to_string(),
                why,
                confirmed,
                timed_out,
            });
        }
    }

    /// #3280 — what the `base` phase came to.
    pub(super) struct BaseOutcome {
        /// `None` = no usable answer, for any reason (spawn failed, out of runs
        /// or time): no hardware, the caller's fallback.
        pub(super) report: Option<ProbeReport>,
        /// `seed`, and what the runs added to it.
        pub(super) denied: Vec<LearnedDeny>,
        /// It left no report, and any of its runs hung or was cut off, or it spent
        /// its budget. A try is worth making. Never true beside a report.
        pub(super) out_of_time: bool,
        /// A session asked, so it stopped where it was.
        pub(super) stopped: bool,
    }

    /// #3267 — the `base` phase: as many runs as it takes to get a report, each
    /// with every cell learned so far denied, within [`MAX_BASE_RUNS`] and
    /// `budget`. It starts with `seed` denied: nothing on a first probe, and on a
    /// later try what is not there for want of time (#3280).
    ///
    /// A report the child printed is the answer however the child ended
    /// afterwards: every open behind it had already come back, so a fault in
    /// its exit (a driver's DLL teardown) has no cell to cost.
    ///
    /// `run` starts one child with the given cells denied. Learning off is the
    /// behaviour before #3267: one run, and a death means no hardware.
    pub(super) fn run_base(
        learning: bool,
        seed: Vec<LearnedDeny>,
        budget: std::time::Duration,
        mut run: impl FnMut(&[LearnedDeny]) -> Option<Run>,
        elapsed: impl Fn() -> std::time::Duration,
    ) -> BaseOutcome {
        let mut retry = BaseRetry {
            budget,
            denied: seed,
            ..Default::default()
        };
        let mut ran_short = false;
        loop {
            retry.runs += 1;
            let Some(child) = run(&retry.denied) else {
                return BaseOutcome {
                    report: None,
                    denied: retry.denied,
                    out_of_time: ran_short,
                    stopped: false,
                };
            };
            ran_short |= ran_out_of_time(&child);
            if matches!(child.ending, Ending::Stopped { .. }) {
                if learning && child.ending.stuck() {
                    stuck_costs_media_foundation(&mut retry.denied);
                }
                return BaseOutcome {
                    report: None,
                    denied: retry.denied,
                    out_of_time: false,
                    stopped: true,
                };
            }
            if (learning || child.ending == Ending::Clean)
                && let Some(report) = parse_line(&child.stdout, child.elapsed)
            {
                if child.ending != Ending::Clean {
                    tracing::warn!(
                        ending = ?child.ending,
                        "caps probe: the base child ended badly after it reported — its report \
                         stands, every open behind it had come back"
                    );
                }
                if retry.runs > 1 {
                    tracing::warn!(
                        runs = retry.runs,
                        denied = ?retry.denied.iter().map(|d| d.cell.as_str()).collect::<Vec<_>>(),
                        "caps probe: the base phase reported after a death"
                    );
                }
                // A report stands, and a child that printed one and then stuck is
                // still a driver stuck in the kernel.
                if learning && child.ending.stuck() {
                    stuck_costs_media_foundation(&mut retry.denied);
                }
                return BaseOutcome {
                    report: Some(report),
                    denied: retry.denied,
                    out_of_time: false,
                    stopped: false,
                };
            }
            if !learning {
                return BaseOutcome {
                    report: None,
                    denied: retry.denied,
                    out_of_time: ran_short,
                    stopped: false,
                };
            }
            let last_open = last_progress(&child.stdout);
            let step = retry.weigh_death(last_open.as_deref(), &child.ending, elapsed());
            if child.ending.stuck() {
                stuck_costs_media_foundation(&mut retry.denied);
            }
            let why = match step {
                Step::RunAgain => continue,
                Step::GiveUp(why) => why,
                Step::OutOfBudget => {
                    ran_short = true;
                    "the time limit"
                }
            };
            tracing::error!(
                why,
                runs = retry.runs,
                elapsed_ms = elapsed().as_millis(),
                denied = ?retry.denied.iter().map(|d| d.cell.as_str()).collect::<Vec<_>>(),
                "caps probe: the base phase left no report — no hardware advertised"
            );
            return BaseOutcome {
                report: None,
                denied: retry.denied,
                out_of_time: ran_short,
                stopped: false,
            };
        }
    }

    /// #3267 — `base` gave up on the hardware, and libvpx needs no driver: one
    /// more child, opening that and nothing else, so the host keeps VP9 4:4:4
    /// over the DataChannel instead of falling to the RTP track. A child all the
    /// same, because this is a host whose probes have been dying and the daemon
    /// has no business finding out why. At most one child's bound, on a path
    /// where no 4:4:4 or GPU child follows, since neither has a candidate left.
    /// A first probe's only: a later try has an answer in force to fall back on.
    fn run_software(
        run: &mut impl FnMut(&[LearnedDeny], &[(&str, String)]) -> Option<Run>,
        learned: &mut Vec<LearnedDeny>,
    ) -> Option<ProbeReport> {
        tracing::warn!(
            "caps probe: no answer from the base phase — asking a child for the cells that need \
             no driver"
        );
        let child = run(learned, &[(PHASE_ENV, "software".to_string())])?;
        if child.ending.stuck() {
            stuck_costs_media_foundation(learned);
        }
        parse_line(&child.stdout, child.elapsed)
    }

    /// #3280 — how long a probe gives its children.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) struct Bounds {
        /// One child's run, before it is told to die.
        pub(super) child: std::time::Duration,
        /// After that: for it to die, and for its stdout to end.
        pub(super) kill_wait: std::time::Duration,
        /// No new `base` run starts this long after the first started.
        pub(super) base_budget: std::time::Duration,
    }

    impl Bounds {
        /// The first probe's, which the device waits on.
        pub(super) const FIRST: Bounds = Bounds {
            child: TIMEOUT,
            kill_wait: KILL_WAIT,
            base_budget: BASE_RETRY_BUDGET,
        };
    }

    /// #3280 — one whole probe, and what it starts from.
    pub(super) struct Try<'a> {
        pub(super) bounds: Bounds,
        /// What its children start with denied. The daemon's own list is not read
        /// or relaxed for this: a session keeps honouring that one.
        pub(super) denied: Vec<LearnedDeny>,
        /// Read on every 50 ms poll of a child. True = a session asked, so the
        /// child is told to die and the probe ends where it is.
        pub(super) stop: &'a dyn Fn() -> bool,
        /// The answer the device starts on, which takes the software child when
        /// `base` leaves nothing ([`run_software`]).
        pub(super) first: bool,
    }

    /// #3280 — what ran out of time, so that more of it might answer.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub(super) struct Shortfall {
        /// `base` left no report ([`BaseOutcome::out_of_time`]): no hardware.
        pub(super) base: bool,
        /// Named cells: denied on a hang or because a child would not die, and the
        /// form a `444` or `gpu` child was inside when it ran out. The same one
        /// again with more time is a real hang.
        pub(super) cells: Vec<String>,
        /// A `444` or `gpu` child ran out of time.
        pub(super) forms: bool,
    }

    impl Shortfall {
        fn of(base: bool, learned: &[LearnedDeny], phases: PhaseGaps) -> Option<Self> {
            let mut cells: Vec<String> = learned
                .iter()
                .filter(|d| d.timed_out)
                .map(|d| d.cell.clone())
                .collect();
            for cell in phases.cells {
                if !cells.contains(&cell) {
                    cells.push(cell);
                }
            }
            (base || phases.forms || !cells.is_empty()).then_some(Shortfall {
                base,
                cells,
                forms: phases.forms,
            })
        }
    }

    /// What the `444` and `gpu` children left missing.
    #[derive(Default)]
    struct PhaseGaps {
        forms: bool,
        cells: Vec<String>,
        /// [`Probed::unanswered`].
        unanswered: bool,
    }

    impl PhaseGaps {
        /// This child left no answer. Only a fault's death says anything about the
        /// host, so anything else keeps the rest of the answer out of the cache:
        /// an OOM kill, an EDR's terminate, a child that could not start. Only
        /// running out of time is worth a try.
        fn note(&mut self, run: Option<&Run>) {
            let out_of_time = run.is_some_and(ran_out_of_time);
            let fault = run.is_some_and(|r| matches!(r.ending, Ending::Died { fault: true, .. }));
            self.unanswered |= out_of_time || !fault;
            if let Some(run) = run.filter(|_| out_of_time) {
                self.forms = true;
                if let Some(cell) = last_progress(&run.stdout)
                    && !self.cells.contains(&cell)
                {
                    self.cells.push(cell);
                }
            }
        }
    }

    /// #3280 — what one probe came to.
    pub(super) struct Probed {
        pub(super) report: Option<ProbeReport>,
        /// [`Try::denied`], and what this probe added to it.
        pub(super) learned: Vec<LearnedDeny>,
        /// What it ran out of time on. `None` is an answer, even "no hardware".
        pub(super) short: Option<Shortfall>,
        /// A `444` or `gpu` child left no answer, and not by a fault, so those
        /// forms are missing for no reason about the host, and the answer must not
        /// outlive this process.
        pub(super) unanswered: bool,
        /// A session asked: the probe ended where it was, and none of this counts.
        pub(super) stopped: bool,
    }

    impl Probed {
        fn stopped(learned: Vec<LearnedDeny>) -> Self {
            Probed {
                report: None,
                learned,
                short: None,
                unanswered: false,
                stopped: true,
            }
        }
    }

    /// The answer a phase's child printed, however it ended afterwards, as with
    /// `base`'s report: every open behind it had already come back. Learning off
    /// is the behaviour before #3267.
    fn printed_answer<T>(
        run: Option<&Run>,
        learning: bool,
        parse: impl Fn(&str) -> Option<T>,
    ) -> Option<T> {
        run.filter(|r| learning || r.ending == Ending::Clean)
            .and_then(|r| parse(&r.stdout))
    }

    /// Probe in up to three phases, each in its own child. No report = no usable
    /// answer from the BASE phase ([`run_base`]) or, after it, the software one
    /// ([`run_software`]) — the caller treats every reason the same way. A
    /// failure of the 4:4:4 phase costs only the 4:4:4 cells: the base matrix
    /// is advertised, with the last open the child reported named in the log
    /// so the operator can denylist it.
    ///
    /// Why two processes (FR-77 P3c, field 2026-09-08): CORPLAP-3's Intel
    /// media runtime died with 0xc0000005 on the first `vp9_qsv` 4:4:4 open
    /// over VUYX, and one child meant the daemon advertised NO hardware at
    /// all — `av1_qsv`, `h264_qsv`, `vp9_qsv` 4:2:0 all gone for a cell the
    /// session pump would never have needed.
    pub(super) fn probe(this: &Try) -> Probed {
        if std::env::var_os(CHILD_ENV).is_some() {
            // We ARE the child (or something re-entered). Probing in-process
            // is what this process was started to do.
            return Probed {
                report: Some(super::in_process_report(phase_from_env() != Phase::Base)),
                learned: Vec::new(),
                short: None,
                unanswered: false,
                stopped: false,
            };
        }
        probe_with(this, cells::learning_enabled(), |list, extra| {
            run_child(this, list, extra)
        })
    }

    /// [`probe`], with the children to run as an argument. `run` starts one with
    /// that list denied and those variables set.
    pub(super) fn probe_with(
        this: &Try,
        learning: bool,
        mut run: impl FnMut(&[LearnedDeny], &[(&str, String)]) -> Option<Run>,
    ) -> Probed {
        let started = std::time::Instant::now();
        let mut gaps = PhaseGaps::default();
        // A later try has an answer in force, so once one of its own children is
        // stuck it starts no more: they could only pile onto the same driver. A
        // first probe has nothing to fall back on, so it goes on.
        let first = this.first;
        let mut stuck = false;
        let mut run = move |list: &[LearnedDeny], extra: &[(&str, String)]| {
            if stuck && !first {
                return None;
            }
            let child = run(list, extra);
            stuck |= child.as_ref().is_some_and(|r| r.ending.stuck());
            child
        };

        let base = run_base(
            learning,
            this.denied.clone(),
            this.bounds.base_budget,
            |denied| {
                // Whatever becomes of this probe, a session opens encoders in the
                // daemon, and the fault that ended the child would end the daemon.
                cells::join_learned(denied.iter().filter(|d| d.confirmed).cloned());
                run(denied, &[(PHASE_ENV, "base".to_string())])
            },
            || started.elapsed(),
        );
        let BaseOutcome {
            report,
            denied: mut learned,
            out_of_time,
            stopped,
        } = base;
        cells::join_learned(learned.iter().filter(|d| d.confirmed).cloned());
        let report = match report {
            Some(report) => Some(report),
            None if stopped => return Probed::stopped(learned),
            None if this.first && learning => run_software(&mut run, &mut learned),
            None => None,
        };
        let Some(mut report) = report else {
            return Probed {
                report: None,
                short: Shortfall::of(out_of_time, &learned, gaps),
                learned,
                unanswered: false,
                stopped: false,
            };
        };

        let deny = cells::denied_with(&learned);
        let names = candidates_444(&report.caps, &deny);
        if names.is_empty() {
            tracing::info!(
                elapsed_ms = started.elapsed().as_millis(),
                "caps probe: no 4:4:4 candidate on this host — single phase"
            );
        } else {
            let child = run(
                &learned,
                &[
                    (PHASE_ENV, "444".to_string()),
                    (NAMES_444_ENV, names.join(",")),
                ],
            );
            if learning && child.as_ref().is_some_and(|r| r.ending.stuck()) {
                stuck_costs_media_foundation(&mut learned);
            }
            if child
                .as_ref()
                .is_some_and(|r| matches!(r.ending, Ending::Stopped { .. }))
            {
                return Probed::stopped(learned);
            }
            match printed_answer(child.as_ref(), learning, parse_444_line) {
                Some(ok) => {
                    tracing::info!(
                        elapsed_ms = child.as_ref().map_or(0, |r| r.elapsed.as_millis()),
                        tried = ?names,
                        opened = ?ok,
                        "caps probe: 4:4:4 phase reported"
                    );
                    merge_444(&mut report, &ok);
                }
                None if child.as_ref().is_some_and(|r| r.ending == Ending::Clean) => {
                    gaps.note(child.as_ref());
                    tracing::warn!(
                        tried = ?names,
                        "caps probe: 4:4:4 phase printed no answer — advertising the 4:2:0 matrix"
                    )
                }
                None => {
                    gaps.note(child.as_ref());
                    let last = child
                        .as_ref()
                        .and_then(|r| last_progress(&r.stdout))
                        .unwrap_or_else(|| "(none reported)".to_string());
                    tracing::error!(
                        tried = ?names,
                        last_open = %last,
                        "caps probe: the 4:4:4 probe child DIED or hung — advertising the \
                         4:2:0 matrix only. The open it was inside is the cell to add to \
                         `encoder_cells_deny` (`<name>:yuv444`); the daemon is unaffected."
                    );
                }
            }
        }

        // FR-118 P2-3 — the GPU-input forms, in a child of their own: a form
        // that kills its process costs only the GPU forms (the 4:4:4
        // lesson), and the daemon never opens one the probe has not proven.
        // With the switch off there are no candidates, so no child (AC6).
        let gpu_names = candidates_gpu(&report.caps, &cells::denied_with(&learned));
        if !gpu_names.is_empty() {
            let child = run(
                &learned,
                &[
                    (PHASE_ENV, "gpu".to_string()),
                    (NAMES_GPU_ENV, gpu_names.join(",")),
                ],
            );
            if learning && child.as_ref().is_some_and(|r| r.ending.stuck()) {
                stuck_costs_media_foundation(&mut learned);
            }
            if child
                .as_ref()
                .is_some_and(|r| matches!(r.ending, Ending::Stopped { .. }))
            {
                return Probed::stopped(learned);
            }
            match printed_answer(child.as_ref(), learning, parse_gpu_line) {
                Some(ok) => {
                    tracing::info!(
                        elapsed_ms = child.as_ref().map_or(0, |r| r.elapsed.as_millis()),
                        tried = ?gpu_names,
                        proven = ?ok,
                        "caps probe: GPU-input phase reported"
                    );
                    report.gpu_input = ok;
                }
                None if child.as_ref().is_some_and(|r| r.ending == Ending::Clean) => {
                    gaps.note(child.as_ref());
                    tracing::warn!(
                        tried = ?gpu_names,
                        "caps probe: GPU-input phase printed no answer — sessions take CPU frames"
                    )
                }
                None => {
                    gaps.note(child.as_ref());
                    let last = child
                        .as_ref()
                        .and_then(|r| last_progress(&r.stdout))
                        .unwrap_or_else(|| "(none reported)".to_string());
                    tracing::error!(
                        tried = ?gpu_names,
                        last_open = %last,
                        "caps probe: the GPU-input probe child DIED or hung — sessions take CPU \
                         frames. The open it was inside is the entry to add to `zero_copy_deny` \
                         (`<name>:gpu`); the daemon is unaffected."
                    );
                }
            }
        }

        if !MF_IN_THE_DAEMON.is_empty()
            && media_foundation_denies()
                .iter()
                .all(|d| learned.iter().any(|l| l.cell == d.cell))
        {
            withdraw_media_foundation(&mut report.caps);
        }

        // The whole cost the daemon paid: every spawn, every open, every parse.
        report.caps.probe_ms =
            Some(u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX));
        tracing::info!(
            elapsed_ms = report.caps.probe_ms.unwrap_or(0),
            codecs = ?report.caps.codecs,
            hw_encoders = ?report.caps.hw_encoders,
            cells = report.caps.video_cells.len(),
            vp9_qsv_idr = ?report.vp9_qsv_idr,
            learned_deny = ?learned.iter().map(|l| l.cell.clone()).collect::<Vec<_>>(),
            "caps probe: child reported"
        );
        let unanswered = gaps.unanswered;
        Probed {
            report: Some(report),
            short: Shortfall::of(out_of_time, &learned, gaps),
            learned,
            unanswered,
            stopped: false,
        }
    }

    /// #3273 — whether a child ended for want of time, rather than with an
    /// answer or a death of its own: it hung, or its stdout outlasted the bound.
    /// What it did not say says nothing about the host.
    pub(super) fn ran_out_of_time(run: &Run) -> bool {
        matches!(run.ending, Ending::Hung { .. }) || run.cut_off
    }

    /// Spawn one `caps-probe` child with `extra` on top of the recursion
    /// guard, the S2 config fallbacks and `list`, and wait for it
    /// ([`wait_for_child`]), within this probe's [`Bounds`]. `None` = could not
    /// spawn.
    fn run_child(this: &Try, list: &[LearnedDeny], extra: &[(&str, String)]) -> Option<Run> {
        if (this.stop)() {
            return Some(Run {
                ending: Ending::Stopped { gone: true },
                stdout: String::new(),
                cut_off: false,
                elapsed: std::time::Duration::ZERO,
            });
        }
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(%e, "caps probe: cannot resolve our own path — skipping HW probes");
                return None;
            }
        };
        let started = std::time::Instant::now();
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("caps-probe")
            .env(CHILD_ENV, "1")
            // S2 config fallbacks are process-local: hand them to the child as
            // real env vars, or every knob it reads falls to its built-in
            // default (FR-19 P4c: `relay-server` was never advertised).
            .envs(tunnel_core::env::config_fallbacks_for_child())
            // #3267 — process-local too, and the deny the next run depends on.
            .envs(cells::list_for_child(list))
            .envs(extra.iter().map(|(k, v)| (*k, v.as_str())))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            // Inherited on purpose: on an attended host the child's own
            // probe logging lands next to the verdict. On a service host it
            // reaches no log — which is what the PROGRESS lines are for.
            .stderr(std::process::Stdio::inherit());
        #[cfg(windows)]
        {
            // No console window when the daemon runs attended.
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let childp = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(%e, "caps probe: could not spawn the probe child — no HW advertisement");
                return None;
            }
        };
        // #2717 — the child ends with this process: a worker told to stop
        // mid-probe leaves without waiting it out (`signaling::run`).
        #[cfg(windows)]
        let _ends_with_us = tie_to_this_process(&childp);

        let bounds = this.bounds;
        let (waited, output) =
            wait_for_child(childp, bounds.child, bounds.kill_wait, this.stop, |c| {
                let _ = c.kill();
            });
        let Output {
            text: stdout,
            cut_off,
        } = output;
        let phase = extra.iter().find(|(k, _)| *k == PHASE_ENV);
        let phase = phase.map(|(_, v)| v.as_str());
        let last_open = last_progress(&stdout).unwrap_or_default();
        if cut_off {
            tracing::warn!(
                kill_wait_s = bounds.kill_wait.as_secs(),
                ?phase,
                %last_open,
                "caps probe: the child's stdout had not ended at the bound, so anything it wrote \
                 after that is lost, a report included. Something it started may hold the pipe, \
                 or this host is too busy to read it in time."
            );
        }
        let status = match waited {
            Waited::Exited(status) => status,
            Waited::Hung { gone } => {
                // A hung driver. Carry on without this phase.
                if gone {
                    tracing::warn!(
                        timeout_s = bounds.child.as_secs(),
                        ?phase,
                        %last_open,
                        "caps probe: the probe child hung — the daemon is unaffected; this is \
                         the process boundary doing its job."
                    );
                } else {
                    tracing::error!(
                        timeout_s = bounds.child.as_secs(),
                        kill_wait_s = bounds.kill_wait.as_secs(),
                        ?phase,
                        %last_open,
                        "caps probe: the probe child hung and did not die when killed. It is \
                         most likely stuck in a kernel call in a driver, and stays until that \
                         call returns. Going on without it; the daemon is unaffected."
                    );
                    stuck_reaches_the_daemon();
                }
                return Some(Run {
                    ending: Ending::Hung { gone },
                    stdout,
                    cut_off,
                    elapsed: started.elapsed(),
                });
            }
            Waited::Stopped { gone } => {
                tracing::info!(
                    gone,
                    ?phase,
                    %last_open,
                    "caps probe: a session asked for this host, so this try's child was told to \
                     stop"
                );
                if !gone {
                    stuck_reaches_the_daemon();
                }
                return Some(Run {
                    ending: Ending::Stopped { gone },
                    stdout,
                    cut_off,
                    elapsed: started.elapsed(),
                });
            }
        };

        if !status.success() {
            // THE case this module exists for. A signal death here is a
            // driver fault that would otherwise have been ours.
            tracing::error!(
                status = %status,
                elapsed_ms = started.elapsed().as_millis(),
                ?phase,
                %last_open,
                "caps probe: the probe child DIED. Before this ran out-of-process the same \
                 fault crash-looped the daemon."
            );
            return Some(Run {
                ending: Ending::Died {
                    status: status.to_string(),
                    fault: is_fault(&status),
                },
                stdout,
                cut_off,
                elapsed: started.elapsed(),
            });
        }
        Some(Run {
            ending: Ending::Clean,
            stdout,
            cut_off,
            elapsed: started.elapsed(),
        })
    }

    /// #3280 — the daemon's own list takes [`media_foundation_denies`] the moment a
    /// child is known to be stuck: a session may be about to open them, and neither
    /// a first probe nor a later try has got as far as an answer.
    fn stuck_reaches_the_daemon() {
        if cells::learning_enabled() {
            cells::join_learned(media_foundation_denies());
        }
    }

    /// The last `PROGRESS` line in a child's output: the open it was inside
    /// when it stopped answering. `None` when it printed none, or the last one
    /// said that open had ended ([`NO_OPEN`]).
    pub(super) fn last_progress(out: &str) -> Option<String> {
        out.lines()
            .rev()
            .find_map(|l| l.trim().strip_prefix(PROGRESS))
            .map(|s| s.trim().to_string())
            .filter(|s| s != NO_OPEN)
    }

    /// FR-77 P3c — the FFmpeg encoder names whose 4:4:4 form the second
    /// phase should try: every hardware cell of the base matrix whose name
    /// the source matrix lists for 4:4:4, minus the denylist. Cells the base
    /// phase could not open are never asked (no `vp9_qsv` 4:4:4 on a host
    /// without `vp9_qsv`). `deny` is this probe's ([`cells::denied_with`]).
    pub(super) fn candidates_444(caps: &AgentCaps, deny: &[String]) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for cell in caps.typed_cells() {
            let backend = match cell.backend {
                VideoBackend::Nvenc
                | VideoBackend::Qsv
                | VideoBackend::Vaapi
                | VideoBackend::Vulkan => cell.backend.wire(),
                _ => continue,
            };
            if cell.codec == VideoCodec::Av1 {
                continue;
            }
            let name = format!("{}_{}", cell.codec.wire(), backend);
            if crate::encode::cells::ffmpeg_444_capable(&name)
                && !crate::encode::cells::cell_denied(deny, &name, ChromaFormat::Yuv444)
                && !names.contains(&name)
            {
                names.push(name);
            }
        }
        names
    }

    /// FR-77 P3c — fold the 4:4:4 phase's answer into the base report: the
    /// named cells gain `yuv444`, and the legacy `hevc_chroma` says 4:4:4
    /// exactly when the HEVC winner is `hevc_nvenc` and it opened (the P7
    /// contract the viewer's legacy derivation reads).
    pub(super) fn merge_444(report: &mut ProbeReport, ok: &[String]) {
        for name in ok {
            let Some((codec, backend)) = VideoBackend::from_ffmpeg_name(name) else {
                continue;
            };
            for cell in report.caps.video_cells.iter_mut() {
                if cell.codec == codec.wire() && cell.backend == backend.wire() {
                    let c444 = ChromaFormat::Yuv444.wire().to_string();
                    if !cell.chroma.contains(&c444) {
                        cell.chroma.push(c444);
                    }
                }
            }
            let hevc_winner_is_nvenc = report
                .caps
                .hw_encoders
                .iter()
                .find(|h| h.starts_with("ffmpeg-hevc_"))
                .is_some_and(|h| h == "ffmpeg-hevc_nvenc");
            if name == "hevc_nvenc"
                && hevc_winner_is_nvenc
                && !report.caps.hevc_chroma.iter().any(|c| c == "yuv444")
            {
                report.caps.hevc_chroma.push("yuv444".into());
            }
        }
    }

    /// The child's marked line → the report, with `probe_ms` stamped by the
    /// PARENT (the caller overwrites it with the whole two-phase cost). A
    /// bare `AgentCaps` line (the P1 shape) still parses, so the two halves
    /// of a binary can never disagree on the envelope.
    pub(super) fn parse_line(out: &str, elapsed: std::time::Duration) -> Option<ProbeReport> {
        let line = out.lines().find_map(|l| l.trim().strip_prefix(MARKER))?;
        let mut report = match serde_json::from_str::<ProbeReport>(line) {
            Ok(r) => r,
            Err(e) => match serde_json::from_str::<AgentCaps>(line) {
                Ok(caps) => ProbeReport {
                    caps,
                    vp9_qsv_idr: None,
                    gpu_input: Vec::new(),
                },
                Err(_) => {
                    tracing::warn!(%e, "caps probe: child output was not parseable — no HW advertisement");
                    return None;
                }
            },
        };
        report.caps.probe_ms = Some(u32::try_from(elapsed.as_millis()).unwrap_or(u32::MAX));
        Some(report)
    }

    /// The 4:4:4 phase's marked line → the names that opened.
    pub(super) fn parse_444_line(out: &str) -> Option<Vec<String>> {
        let line = out
            .lines()
            .find_map(|l| l.trim().strip_prefix(MARKER_444))?;
        serde_json::from_str::<Vec<String>>(line).ok()
    }

    /// FR-118 P2-3 — the GPU-input phase's marked line → the names proven.
    pub(super) fn parse_gpu_line(out: &str) -> Option<Vec<String>> {
        let line = out
            .lines()
            .find_map(|l| l.trim().strip_prefix(MARKER_GPU))?;
        serde_json::from_str::<Vec<String>>(line).ok()
    }

    /// FR-118 P2-3 — the encoders whose GPU-input form the third phase should
    /// try: the base matrix's QSV and (P3) NVENC cells the GPU input covers
    /// (4:2:0), minus the cell denylist and `zero_copy_deny`; none at all with
    /// the switch off or on a build without GPU frames. A gate the probe
    /// honours is a gate only if the session honours it too: `gpu_input::plan`
    /// reads the same two lists at every open. `deny` is this probe's
    /// ([`cells::denied_with`]).
    pub(super) fn candidates_gpu(caps: &AgentCaps, deny: &[String]) -> Vec<String> {
        #[cfg(all(
            target_os = "windows",
            feature = "ffmpeg-encoder",
            any(feature = "mf-encoder", feature = "wgc-capture")
        ))]
        {
            if !crate::capture::gpu_frame::zero_copy_enabled() {
                return Vec::new();
            }
            let gpu_deny = crate::encode::ffmpeg::gpu_hint::denied();
            let mut names: Vec<String> = Vec::new();
            for cell in caps.typed_cells() {
                if !matches!(cell.backend, VideoBackend::Qsv | VideoBackend::Nvenc) {
                    continue;
                }
                let name = format!("{}_{}", cell.codec.wire(), cell.backend.wire());
                if crate::encode::ffmpeg::gpu_input::gpu_cell(&name, false)
                    && !crate::encode::cells::cell_denied(deny, &name, ChromaFormat::Yuv420)
                    && !crate::encode::ffmpeg::gpu_hint::is_denied(&gpu_deny, &name)
                    && !names.contains(&name)
                {
                    names.push(name);
                }
            }
            names
        }
        // FR-118 P5 — VideoToolbox's 4:2:0 cells, under the same switch and the
        // same two denylists `vt_input::plan` reads at every open.
        #[cfg(all(
            target_os = "macos",
            feature = "ffmpeg-encoder",
            feature = "scrap-capture"
        ))]
        {
            if !crate::capture::zero_copy_enabled() {
                return Vec::new();
            }
            let gpu_deny = crate::encode::ffmpeg::gpu_hint::denied();
            let mut names: Vec<String> = Vec::new();
            for cell in caps.typed_cells() {
                if !matches!(cell.backend, VideoBackend::VideoToolbox) {
                    continue;
                }
                let name = format!("{}_{}", cell.codec.wire(), cell.backend.wire());
                if crate::encode::ffmpeg::vt_input::gpu_cell(&name, false)
                    && !crate::encode::cells::cell_denied(deny, &name, ChromaFormat::Yuv420)
                    && !crate::encode::ffmpeg::gpu_hint::is_denied(&gpu_deny, &name)
                    && !names.contains(&name)
                {
                    names.push(name);
                }
            }
            names
        }
        #[cfg(not(any(
            all(
                target_os = "windows",
                feature = "ffmpeg-encoder",
                any(feature = "mf-encoder", feature = "wgc-capture")
            ),
            all(
                target_os = "macos",
                feature = "ffmpeg-encoder",
                feature = "scrap-capture"
            )
        )))]
        {
            let _ = (caps, deny);
            Vec::new()
        }
    }

    /// How waiting on a child came out.
    #[derive(Debug)]
    pub(super) enum Waited {
        /// It ended by itself.
        Exited(std::process::ExitStatus),
        /// Still running at the limit, and told to die. `gone` = it did.
        Hung { gone: bool },
        /// #3280 — `stop` said so, and it was told to die. `gone` = it did.
        Stopped { gone: bool },
    }

    /// #3280 — children told to die that had not within their bound, until the call
    /// they are stuck in returns. No later try starts while this is above 0: a
    /// second child could only pile onto the same stuck driver.
    static STUCK: AtomicU32 = AtomicU32::new(0);

    pub(super) fn stuck_children() -> u32 {
        STUCK.load(Ordering::SeqCst)
    }

    /// #3273 — what [`collect_output`] got of a child's stdout.
    #[derive(Debug)]
    pub(super) struct Output {
        pub(super) text: String,
        /// The pipe had not ended by the deadline.
        pub(super) cut_off: bool,
    }

    /// #3273 — wait for a child and read what it wrote, for at most `timeout`
    /// plus `kill_wait`, whatever the child does.
    ///
    /// Neither wait here ends by itself. A child stuck in a kernel call does not
    /// die when killed: the kill is queued until the call returns, and a GPU
    /// driver hung in a kernel wait is the way to get there. Its stdout does not
    /// end while anything holds the pipe's write end: that child, or something
    /// it started, which can outlive a child that exited cleanly. Nothing
    /// restarts a worker during a probe (`signaling::run` gates the watchdog's
    /// pump off), so either wait, unbounded, keeps the device offline until
    /// someone acts on the host.
    ///
    /// `stop` ends the wait early, on a later try when a session asks (#3280).
    /// `kill` is a parameter because no real process can ignore a kill, so a test
    /// has to decline to send it.
    pub(super) fn wait_for_child(
        mut child: std::process::Child,
        timeout: std::time::Duration,
        kill_wait: std::time::Duration,
        stop: &dyn Fn() -> bool,
        kill: impl FnOnce(&mut std::process::Child),
    ) -> (Waited, Output) {
        // Read stdout on its own thread while waiting, so a chatty child cannot
        // fill the pipe and deadlock against our own wait.
        let output = read_output(child.stdout.take());
        let ended = wait_bounded(&mut child, timeout, stop);
        if let WaitEnd::Exited(status) = ended {
            let deadline = std::time::Instant::now() + kill_wait;
            return (Waited::Exited(status), collect_output(output, deadline));
        }
        kill(&mut child);
        let deadline = std::time::Instant::now() + kill_wait;
        let gone = matches!(
            wait_bounded(&mut child, kill_wait, &|| false),
            WaitEnd::Exited(_)
        );
        if !gone {
            // Reap it if that call ever returns, or it stays a zombie for as
            // long as the daemon runs. On a host out of threads it goes unreaped,
            // which is no reason to take the probe down. It also stays counted
            // then: nothing will ever say that call returned.
            STUCK.fetch_add(1, Ordering::SeqCst);
            let reaper = std::thread::Builder::new()
                .name("roomlerd-caps-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                    STUCK.fetch_sub(1, Ordering::SeqCst);
                });
            if let Err(e) = reaper {
                tracing::warn!(%e, "caps probe: no thread to reap the stuck child");
            }
        }
        let waited = match ended {
            WaitEnd::Stopped => Waited::Stopped { gone },
            _ => Waited::Hung { gone },
        };
        (waited, collect_output(output, deadline))
    }

    /// #3273 — a piece of a child's stdout, and when it arrived.
    pub(super) type Chunk = (std::time::Instant, Vec<u8>);

    /// #3273 — a child's stdout, chunk by chunk as it arrives. The thread ends
    /// with the pipe, or at the first chunk after the receiver has gone.
    fn read_output(stdout: Option<std::process::ChildStdout>) -> std::sync::mpsc::Receiver<Chunk> {
        use std::io::Read;
        let (chunks, output) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("roomlerd-caps-read".into())
            .spawn(move || {
                let Some(mut stdout) = stdout else {
                    return;
                };
                let mut chunk = [0u8; 4096];
                loop {
                    match stdout.read(&mut chunk) {
                        Ok(0) => return,
                        Ok(n) => {
                            let arrived = std::time::Instant::now();
                            if chunks.send((arrived, chunk[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => return,
                    }
                }
            });
        // With no thread the sender is gone too, and this reads as a child that
        // wrote nothing: the safe answer, and no panic on a host out of threads.
        if let Err(e) = reader {
            tracing::warn!(%e, "caps probe: no thread to read the child's stdout");
        }
        output
    }

    /// #3273 — what [`read_output`] has read by `deadline`, or by the end of the
    /// pipe if that comes first.
    ///
    /// By when a chunk ARRIVED, not by when this gets to it. A child that floods
    /// its pipe never lets the wait below time out, so a chunk from after the
    /// deadline has to end the read. And a stuck child's lines are already queued
    /// when this starts, with the deadline spent on the kill: all of them count,
    /// because the last `PROGRESS` line is the one that names its cell.
    ///
    /// Lossy, because a driver's banner in the wrong code page should cost that
    /// banner and not the report beside it. A pipe that has not ended may have
    /// stopped mid-line, and half a `PROGRESS` line names a cell that does not
    /// exist, so what it gives back then stops at the last whole line.
    pub(super) fn collect_output(
        output: std::sync::mpsc::Receiver<Chunk>,
        deadline: std::time::Instant,
    ) -> Output {
        use std::sync::mpsc::RecvTimeoutError;
        let mut bytes = Vec::new();
        let mut cut_off = false;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match output.recv_timeout(left) {
                Ok((arrived, _)) if arrived > deadline => {
                    cut_off = true;
                    break;
                }
                Ok((_, chunk)) => bytes.extend_from_slice(&chunk),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    cut_off = true;
                    break;
                }
            }
        }
        if cut_off {
            let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            bytes.truncate(whole);
        }
        Output {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            cut_off,
        }
    }

    /// How [`wait_bounded`] ended.
    enum WaitEnd {
        Exited(std::process::ExitStatus),
        /// Still running when time ran out.
        TimedOut,
        /// Still running when `stop` said so.
        Stopped,
    }

    /// `wait` with a limit.
    ///
    /// Polling rather than a platform wait-with-timeout: it is a handful of
    /// wakeups on a path that runs at most a few times per process, and it keeps
    /// this module free of per-platform process APIs.
    fn wait_bounded(
        child: &mut std::process::Child,
        limit: std::time::Duration,
        stop: &dyn Fn() -> bool,
    ) -> WaitEnd {
        let deadline = std::time::Instant::now() + limit;
        loop {
            match child.try_wait() {
                Ok(Some(s)) => return WaitEnd::Exited(s),
                // Treat "cannot ask" as death; the caller's fallback is the
                // safe answer either way.
                Err(_) => return WaitEnd::TimedOut,
                Ok(None) => {}
            }
            if std::time::Instant::now() >= deadline {
                return WaitEnd::TimedOut;
            }
            if stop() {
                return WaitEnd::Stopped;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// #2717 — a job that ends the probe child when this process ends, held
    /// for as long as [`run_child`] waits on it.
    ///
    /// A worker told to stop while it probes leaves without waiting the probe
    /// out (`signaling::run`). Untied, its probe child, a `roomlerd.exe`,
    /// would outlive it. It could hold the very binary an update is replacing,
    /// or, hung in a driver, run on with nobody left to enforce its bound.
    /// Windows has no process group to take down with a parent; a
    /// `KILL_ON_JOB_CLOSE` job is the equivalent, and its last handle closes
    /// when this process ends. `None` (logged) when the job cannot be made:
    /// the probe then runs as it always did.
    #[cfg(windows)]
    pub(super) fn tie_to_this_process(
        child: &std::process::Child,
    ) -> Option<crate::win_service::supervisor::JobObject> {
        use std::os::windows::io::AsRawHandle;
        let job = match crate::win_service::supervisor::JobObject::kill_on_close() {
            Ok(job) => job,
            Err(e) => {
                tracing::warn!(%e, "caps probe: no job for the probe child; a stop mid-probe can leave it running");
                return None;
            }
        };
        // SAFETY: both handles are live: the job's is ours, and the child's
        // process handle lives as long as `child`.
        let joined = unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                job.raw(),
                child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            )
        };
        if joined == 0 {
            tracing::warn!(
                err = ?std::io::Error::last_os_error().raw_os_error(),
                "caps probe: the probe child could not join its job; a stop mid-probe can leave it running"
            );
            return None;
        }
        Some(job)
    }
}

/// The `caps-probe` child's entire job: probe, print one marked JSON line.
///
/// Printing a MARKED line rather than bare JSON so that anything else on
/// stdout — a library that logs there, a driver that prints a banner — cannot
/// be mistaken for the result.
pub fn print_probe_result() {
    match child::phase_from_env() {
        child::Phase::OnlyGpu => {
            let names = child::names_gpu_from_env();
            let proven = probe_gpu(&names);
            match serde_json::to_string(&proven) {
                Ok(json) => child::emit(&format!("{}{json}", child::MARKER_GPU)),
                Err(e) => {
                    tracing::error!(%e, "caps probe: could not serialise the GPU-input answer");
                    std::process::exit(2);
                }
            }
        }
        child::Phase::Only444 => {
            let names = child::names_444_from_env();
            let opened = probe_444(&names);
            match serde_json::to_string(&opened) {
                Ok(json) => child::emit(&format!("{}{json}", child::MARKER_444)),
                Err(e) => {
                    tracing::error!(%e, "caps probe: could not serialise the 4:4:4 answer");
                    std::process::exit(2);
                }
            }
        }
        phase => {
            let report = if phase == child::Phase::Software {
                software_report()
            } else {
                in_process_report(phase != child::Phase::Base)
            };
            match serde_json::to_string(&report) {
                Ok(json) => child::emit(&format!("{}{json}", child::MARKER)),
                Err(e) => {
                    tracing::error!(%e, "caps probe: could not serialise caps");
                    std::process::exit(2);
                }
            }
        }
    }
}

/// FR-77 P3c — the 4:4:4 phase: open the 4:4:4 form of each named encoder,
/// announcing every attempt on stdout first so a dying process leaves the
/// name of the open it was inside. Returns the names that opened.
#[cfg(feature = "ffmpeg-encoder")]
fn probe_444(names: &[String]) -> Vec<String> {
    use crate::encode::ffmpeg::FfmpegEncoder;
    let mut opened = Vec::new();
    if !crate::encode::ffmpeg::available() {
        return opened;
    }
    for name in names {
        // The cascade tables own the `&'static str`; a name outside them is
        // not something this build can open anyway.
        let Some(name) = FfmpegEncoder::static_name(name) else {
            continue;
        };
        child::emit(&format!("{}{name}:yuv444", child::PROGRESS));
        let t = std::time::Instant::now();
        match FfmpegEncoder::new_named_probe(name, PROBE_WIDTH, PROBE_HEIGHT, true) {
            Ok(enc) => {
                drop(enc);
                tracing::info!(
                    encoder = name,
                    elapsed_ms = t.elapsed().as_millis(),
                    "caps probe: 4:4:4 cell opened"
                );
                opened.push(name.to_string());
            }
            Err(e) => tracing::info!(
                encoder = name,
                %e,
                elapsed_ms = t.elapsed().as_millis(),
                "caps probe: 4:4:4 cell did not open — advertising 4:2:0 only"
            ),
        }
    }
    opened
}

#[cfg(not(feature = "ffmpeg-encoder"))]
fn probe_444(_names: &[String]) -> Vec<String> {
    Vec::new()
}

/// FR-118 P2-3 — the GPU-input phase: open each named encoder's GPU-input form
/// on a device of the adapter its frames would come from, and encode two GPU
/// frames through it. QSV's forms open on the adapter the SystemContext DXGI
/// arm captures on (the primary output's); P3, NVENC's on the NVIDIA adapter,
/// where WGC puts its device for them (`gpu_frame::capture_adapter`). A name is
/// proven only if the encoder took the GPU input (not a fallback to CPU frames)
/// and packets came out. Every attempt is announced on stdout first, so a dying
/// process names the open it was inside (`<name>:gpu`, the `zero_copy_deny`
/// entry).
#[cfg(all(
    target_os = "windows",
    feature = "system-context",
    feature = "mf-encoder",
    feature = "scrap-capture",
    feature = "ffmpeg-encoder"
))]
fn probe_gpu(names: &[String]) -> Vec<String> {
    use crate::capture::gpu_frame::TexturePool;
    use crate::encode::ffmpeg::FfmpegEncoder;
    use windows::Win32::Graphics::Direct3D11::ID3D11Device;
    let mut proven = Vec::new();
    if !crate::encode::ffmpeg::available() {
        return proven;
    }
    // One device, with its texture pool, per adapter the frames would come
    // from, made on first need: QSV's on the primary output's adapter, NVENC's
    // on the NVIDIA adapter (P3). `None` inside = no such device here.
    type Source = Option<(ID3D11Device, TexturePool)>;
    let make = |device: Option<ID3D11Device>| -> Source {
        let device = device?;
        match TexturePool::new(&device) {
            Ok(pool) => Some((device, pool)),
            Err(e) => {
                tracing::info!(%e, "caps probe: no texture pool on that adapter's device — no GPU-input form there");
                None
            }
        }
    };
    let mut qsv_source: Option<Source> = None;
    let mut nvenc_source: Option<Source> = None;
    for name in names {
        let Some(name) = FfmpegEncoder::static_name(name) else {
            continue;
        };
        let source = if name.ends_with("_nvenc") {
            nvenc_source
                .get_or_insert_with(|| make(crate::capture::gpu_frame::device_on_vendor(0x10de)))
        } else {
            qsv_source.get_or_insert_with(|| {
                make(crate::system_context::dxgi_direct::primary_output_device())
            })
        };
        let Some((device, pool)) = source.as_ref() else {
            tracing::info!(
                encoder = name,
                "caps probe: no device on this encoder's adapter — no GPU-input form"
            );
            continue;
        };
        child::emit(&format!("{}{name}:gpu", child::PROGRESS));
        let t = std::time::Instant::now();
        let encoded = crate::encode::ffmpeg::gpu_hint::opening_on(Some(device.clone()), || {
            FfmpegEncoder::new_named_probe(name, PROBE_WIDTH, PROBE_HEIGHT, false)
        })
        .and_then(|mut encoder| {
            let input = encoder.rate_stats().input;
            if !input.gpu {
                anyhow::bail!("it opened on CPU frames ({})", input.reason);
            }
            let mut bytes = 0usize;
            for i in 0..2u64 {
                let frame = probe_gpu_frame(device, pool, i)?;
                bytes += encoder
                    .encode_sync(&frame)?
                    .iter()
                    .map(|p| p.data.len())
                    .sum::<usize>();
            }
            if bytes == 0 {
                anyhow::bail!("two GPU frames encoded to nothing");
            }
            Ok(bytes)
        });
        match encoded {
            Ok(bytes) => {
                tracing::info!(
                    encoder = name,
                    bytes,
                    elapsed_ms = t.elapsed().as_millis(),
                    "caps probe: GPU-input form proven"
                );
                proven.push(name.to_string());
            }
            Err(e) => tracing::info!(
                encoder = name,
                %e,
                elapsed_ms = t.elapsed().as_millis(),
                "caps probe: GPU-input form not proven — sessions take CPU frames"
            ),
        }
    }
    proven
}

/// A probe-sized GPU frame on `device`: a gradient that moves with `index`,
/// so the second frame is not a copy of the first.
#[cfg(all(
    target_os = "windows",
    feature = "system-context",
    feature = "mf-encoder",
    feature = "scrap-capture",
    feature = "ffmpeg-encoder"
))]
fn probe_gpu_frame(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    pool: &crate::capture::gpu_frame::TexturePool,
    index: u64,
) -> anyhow::Result<crate::capture::Frame> {
    use crate::capture::{Damage, Frame, FrameData, PixelFormat};
    let (w, h) = (PROBE_WIDTH, PROBE_HEIGHT);
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let v = ((x + y + index as u32 * 16) & 0xff) as u8;
            bgra.extend_from_slice(&[v, v.wrapping_mul(3), 255 - v, 0xff]);
        }
    }
    let texture = pool.take(w, h)?;
    // SAFETY: `bgra` holds `h` rows of `w × 4` bytes, the texture's dims.
    unsafe {
        device
            .GetImmediateContext()
            .map_err(|e| anyhow::anyhow!("GetImmediateContext: {e}"))?
            .UpdateSubresource(&texture, 0, None, bgra.as_ptr().cast(), w * 4, 0);
    }
    Ok(Frame {
        width: w,
        height: h,
        stride: w * 4,
        pixel_format: PixelFormat::Bgra,
        data: FrameData::Gpu(pool.frame(texture, w, h)),
        monotonic_us: index * 16_667,
        monitor: 0,
        damage: Damage::Unknown,
        source: None,
    })
}

/// FR-118 P5 — VideoToolbox's GPU form, proven in the `gpu` child: each cell
/// opened in that form and fed IOSurface frames until it hands back a packet.
/// VideoToolbox encodes asynchronously, so the first frames may come back
/// empty; up to `GPU_PROBE_FRAMES` are fed before the form is called unproven.
#[cfg(all(
    target_os = "macos",
    feature = "scrap-capture",
    feature = "ffmpeg-encoder"
))]
fn probe_gpu(names: &[String]) -> Vec<String> {
    use crate::encode::ffmpeg::FfmpegEncoder;
    const GPU_PROBE_FRAMES: u64 = 10;
    let mut proven = Vec::new();
    if !crate::encode::ffmpeg::available() {
        return proven;
    }
    for name in names {
        let Some(name) = FfmpegEncoder::static_name(name) else {
            continue;
        };
        child::emit(&format!("{}{name}:gpu", child::PROGRESS));
        let t = std::time::Instant::now();
        let encoded = FfmpegEncoder::new_named_probe(name, PROBE_WIDTH, PROBE_HEIGHT, false)
            .and_then(|mut encoder| {
                let input = encoder.rate_stats().input;
                if !input.gpu {
                    anyhow::bail!("it opened on CPU frames ({})", input.reason);
                }
                let mut bytes = 0usize;
                for i in 0..GPU_PROBE_FRAMES {
                    let frame = probe_iosurface_frame(i)?;
                    bytes += encoder
                        .encode_sync(&frame)?
                        .iter()
                        .map(|p| p.data.len())
                        .sum::<usize>();
                    if bytes > 0 {
                        break;
                    }
                }
                if bytes == 0 {
                    anyhow::bail!("{GPU_PROBE_FRAMES} IOSurface frames encoded to nothing");
                }
                Ok(bytes)
            });
        match encoded {
            Ok(bytes) => {
                tracing::info!(
                    encoder = name,
                    bytes,
                    elapsed_ms = t.elapsed().as_millis(),
                    "caps probe: GPU-input form proven"
                );
                proven.push(name.to_string());
            }
            Err(e) => tracing::info!(
                encoder = name,
                %e,
                elapsed_ms = t.elapsed().as_millis(),
                "caps probe: GPU-input form not proven — sessions take CPU frames"
            ),
        }
    }
    proven
}

/// FR-118 P5 — a probe frame on an IOSurface: a gradient that moves with
/// `index`, so consecutive frames differ.
#[cfg(all(
    target_os = "macos",
    feature = "scrap-capture",
    feature = "ffmpeg-encoder"
))]
fn probe_iosurface_frame(index: u64) -> anyhow::Result<crate::capture::Frame> {
    use crate::capture::{Damage, Frame, FrameData, PixelFormat};
    let (w, h) = (PROBE_WIDTH, PROBE_HEIGHT);
    let surface = crate::capture::iosurface_frame::new_bgra(w, h, |x, y, c| {
        let v = ((x + y + index as u32 * 16) & 0xff) as u8;
        match c {
            0 => v,
            1 => v.wrapping_mul(3),
            2 => 255 - v,
            _ => 0xff,
        }
    })?;
    Ok(Frame {
        width: w,
        height: h,
        stride: surface.stride(),
        pixel_format: PixelFormat::Bgra,
        data: FrameData::IoSurface(surface),
        monotonic_us: index * 16_667,
        monitor: 0,
        damage: Damage::Unknown,
        source: None,
    })
}

#[cfg(not(any(
    all(
        target_os = "windows",
        feature = "system-context",
        feature = "mf-encoder",
        feature = "scrap-capture",
        feature = "ffmpeg-encoder"
    ),
    all(
        target_os = "macos",
        feature = "scrap-capture",
        feature = "ffmpeg-encoder"
    )
)))]
fn probe_gpu(_names: &[String]) -> Vec<String> {
    Vec::new()
}

/// Probe dimensions for codec activation checks (HEVC, AV1, VP9-444).
/// Even number, small enough that any encoder accepts it, matching
/// what the internal `probe_pipeline` uses for MFT output
/// verification. Used by the MF cascade probes (Windows-only,
/// `mf-encoder` feature) and the libvpx VP9-444 probe (any platform
/// with `vp9-444` feature). The `dead_code` allowance covers builds
/// that compile in neither feature group.
#[allow(dead_code)]
const PROBE_WIDTH: u32 = 480;
#[allow(dead_code)]
const PROBE_HEIGHT: u32 = 270;

/// FR-118 P2-3 — whether this process is a `caps-probe` child: the one place
/// an encoder form may open before a probe proved it (proving it is the
/// child's job).
pub(crate) fn in_probe_child() -> bool {
    std::env::var_os(child::CHILD_ENV).is_some()
}

/// Detect the codecs and HW backends compiled into this agent build
/// and currently functional on this host. First call runs the
/// activation probes (~300ms per codec on HEVC/AV1-capable boxes,
/// <10ms on boxes with no HW encoder); subsequent calls return the
/// cached result, or a later try's if one was adopted (#3280). This never waits
/// for a try.
pub fn detect() -> AgentCaps {
    let first = CACHED_CAPS.get_or_init(|| {
        let caps = cached_or_probed();
        let short = FIRST_SHORT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .filter(|_| retry_enabled());
        assigned_by_the_daemon(
            caps,
            &HostFacts::here(),
            &super::cells::learned(),
            short.as_ref(),
        )
    });
    late_caps().unwrap_or_else(|| first.clone())
}

/// What the daemon says over whatever `cached_or_probed` answered, on every
/// path. A named seam, like [`with_config_lists`], so a test can lock each line.
fn assigned_by_the_daemon(
    caps: AgentCaps,
    facts: &HostFacts,
    learned: &[super::cells::LearnedDeny],
    short: Option<&child::Shortfall>,
) -> AgentCaps {
    with_learned_word(
        with_honest_words(with_config_lists(caps), facts),
        learned,
        short,
    )
}

/// #3267 — the key and code a learned deny rides in [`AgentCaps::unavailable`].
/// That map is already on the wire and in `agents.capabilities`, and readers
/// skip a key they do not know, so a fleet read sees this on any server, and
/// no viewer says anything about it. `scripts/fr97-caps-census.sh` counts it.
pub const LEARNED_WORD: &str = "encoder-cells";
pub const LEARNED_CODE: &str = "learned-deny";
/// #3280 — the same key's code on a host short of time with no cell to name, so
/// that one with no hardware says why instead of looking like it has none.
pub const SHORT_CODE: &str = "probe-short";

/// #3267 — a host that lost a cell to a driver fault says so: which cell, and
/// how its probe child ended. The collapse this replaces went unnoticed for
/// hours because the viewer still looked healthy.
fn with_learned_word(
    mut caps: AgentCaps,
    learned: &[super::cells::LearnedDeny],
    short: Option<&child::Shortfall>,
) -> AgentCaps {
    use roomler_ai_remote_control::models::Unavailable;
    if learned.is_empty() {
        if short.is_some() {
            caps.unavailable.insert(
                LEARNED_WORD.into(),
                Unavailable::new(
                    SHORT_CODE,
                    "The capability probe ran out of time on this host, so hardware encoders may \
                     be missing from this answer. A later try with more time, or the next start, \
                     can bring them back",
                ),
            );
        }
        return caps;
    }
    let cells = learned
        .iter()
        .map(|l| {
            if l.confirmed {
                format!("{} ({})", l.cell, l.why)
            } else if l.timed_out && short.is_some() {
                format!(
                    "{} ({}, until a later try or the next start)",
                    l.cell, l.why
                )
            } else {
                format!("{} ({}, until the next start)", l.cell, l.why)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    caps.unavailable.insert(
        LEARNED_WORD.into(),
        Unavailable::new(
            LEARNED_CODE,
            &format!(
                "These encoder cells ended the capability probe on this host, so neither the \
                 probe nor a session opens them: {cells}. A driver update, an agent update, a \
                 settings change or a restart a day later tests them again"
            ),
        ),
    );
    caps
}

/// FR-97 P0c — what this host can actually serve, as plain facts, so the
/// decision ([`with_honest_words`]) runs, and is tested, on every OS (FR-43's
/// practice: the decision stays out of `cfg(target_os)`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct HostFacts {
    pub(crate) os: roomler_ai_remote_control::models::OsKind,
    /// The `audio` feature is compiled in.
    pub(crate) audio_built: bool,
    /// This host can capture the computer's own sound:
    ///
    /// - Windows: always (WASAPI loopback).
    /// - Linux: when `audio_source::linux_route` finds a way to it: the
    ///   default output's monitor through Pulse's client library (FR-97
    ///   P3c-2), or a capture device the owner named. Never the default input,
    ///   which is a microphone (P3c-1).
    /// - macOS (FR-97 P2a): ScreenCaptureKit, in a process with a
    ///   window-server session, unless its kill switch is off
    ///   ([`mac_audio_refusal`]).
    pub(crate) audio_backend: bool,
    /// Linux: why there is no way to the computer's sound, by the route's own
    /// code ([`linux_audio_refusal`]). `None` elsewhere, and where there is one.
    pub(crate) linux_audio_refusal: Option<crate::audio_source::AudioUnavailableCode>,
    /// macOS (FR-97 P2a): why this process cannot capture the computer's
    /// sound: `NoSession` (no window-server session, a supervised Mac's root
    /// half) or `SwitchedOff` (`ROOMLERD_SCK_AUDIO=0`). `None` elsewhere, and
    /// where it can.
    pub(crate) mac_audio_refusal: Option<crate::audio_source::AudioUnavailableCode>,
    /// The clipboard watcher reports copied images: Windows, (FR-97 P3e-3a)
    /// a Linux X11 session whose server takes the change gate
    /// (`clipboard::x11_gate`), and (P2e-1) a Mac, behind its change count
    /// (`clipboard::mac_gate`); `clipboard::install_watcher` drops image
    /// events elsewhere.
    pub(crate) watches_clipboard_images: bool,
    /// This process has a clipboard to reach at all.
    pub(crate) clipboard_reachable: bool,
    /// The keyboard layout is this process's to read and switch: on Windows
    /// always; on Linux in an X11 session (FR-97 P3e,
    /// `input::layout_x11::offered`); on macOS (P2d-2a) wherever its kill
    /// switch is on and the build runs AppKit's main loop
    /// (`input::layout_mac::offered`).
    pub(crate) layout_reachable: bool,
    /// A layout switch stays switched: not under GNOME, which puts its own
    /// back (FR-97 P3e, `input::layout_x11::switchable`). There the chip
    /// reports and the picker is not offered.
    pub(crate) layout_switchable: bool,
    /// Display match can switch this host's display (FR-97 P3e-2): Windows;
    /// on Linux a person's own X11 display or the daemon's own virtual desktop
    /// on Xorg's dummy driver; (P2d-2b) a Mac, unless its kill switch is off.
    pub(crate) display_switchable: bool,
}

impl HostFacts {
    pub(crate) fn here() -> Self {
        use roomler_ai_remote_control::models::OsKind;
        let linux_audio_refusal = linux_audio_refusal();
        let mac_audio_refusal = mac_audio_refusal();
        Self {
            os: if cfg!(target_os = "windows") {
                OsKind::Windows
            } else if cfg!(target_os = "macos") {
                OsKind::Macos
            } else {
                OsKind::Linux
            },
            audio_built: cfg!(feature = "audio"),
            audio_backend: cfg!(target_os = "windows")
                || (cfg!(target_os = "linux") && linux_audio_refusal.is_none())
                || (cfg!(target_os = "macos") && mac_audio_refusal.is_none()),
            linux_audio_refusal,
            mac_audio_refusal,
            watches_clipboard_images: watches_clipboard_images(),
            clipboard_reachable: clipboard_reachable(),
            layout_reachable: layout_reachable(),
            layout_switchable: layout_switchable(),
            display_switchable: display_switchable(),
        }
    }
}

/// FR-97 P3e — see [`HostFacts::layout_switchable`].
fn layout_switchable() -> bool {
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    {
        crate::input::layout_x11::switchable()
    }
    #[cfg(all(target_os = "macos", feature = "enigo-input"))]
    {
        crate::input::layout_mac::switchable()
    }
    #[cfg(not(all(any(target_os = "linux", target_os = "macos"), feature = "enigo-input")))]
    {
        cfg!(target_os = "windows")
    }
}

/// FR-97 P3e — see [`HostFacts::layout_reachable`]. Under Wayland the layout
/// is the compositor's, and a system install has no X connection at all.
/// P2d-2a — a Mac's needs AppKit's main loop.
fn layout_reachable() -> bool {
    #[cfg(all(target_os = "linux", feature = "enigo-input"))]
    {
        crate::input::layout_x11::offered()
    }
    #[cfg(all(target_os = "macos", feature = "enigo-input"))]
    {
        crate::input::layout_mac::offered()
    }
    #[cfg(not(all(any(target_os = "linux", target_os = "macos"), feature = "enigo-input")))]
    {
        cfg!(target_os = "windows")
    }
}

/// FR-97 P3e-3a — see [`HostFacts::watches_clipboard_images`]. Asks the X
/// server once (a connection and XFixes' version), for the hello. P2e-1 — a
/// Mac asks only its kill switch.
fn watches_clipboard_images() -> bool {
    #[cfg(all(target_os = "linux", feature = "clipboard"))]
    {
        crate::clipboard::x11_gate::available()
    }
    #[cfg(all(target_os = "macos", feature = "clipboard"))]
    {
        crate::clipboard::mac_gate::available()
    }
    #[cfg(not(all(any(target_os = "linux", target_os = "macos"), feature = "clipboard")))]
    {
        cfg!(target_os = "windows")
    }
}

/// FR-97 P3e-2 — see [`HostFacts::display_switchable`]. A virtual desktop is
/// up before the hello (`main.rs` starts it before connecting).
fn display_switchable() -> bool {
    #[cfg(target_os = "linux")]
    {
        crate::display_match::x11_offered()
            || crate::virtual_desktop::switchable_display().is_some()
    }
    // FR-97 P2d-2b — a Mac, wherever its kill switch is on.
    #[cfg(target_os = "macos")]
    {
        crate::display_match::mac_offered()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        cfg!(target_os = "windows")
    }
}

/// FR-97 P3c-2 — Linux: why a session could not reach the computer's sound,
/// by the route the capture itself takes, or `None` when it can. Asked of the
/// disk (`audio::pulse::present`): the hello loads no library to answer.
fn linux_audio_refusal() -> Option<crate::audio_source::AudioUnavailableCode> {
    #[cfg(all(target_os = "linux", feature = "audio"))]
    {
        use crate::audio_source::{linux_facts, linux_route};
        linux_route(linux_facts(crate::audio::pulse::present()))
            .err()
            .map(|r| r.code)
    }
    #[cfg(not(all(target_os = "linux", feature = "audio")))]
    {
        None
    }
}

/// FR-97 P3c — the hello's word for a Linux host with no way to the computer's
/// sound, by why. Before P3c-1 this track carried the microphone.
fn linux_audio_word(
    why: Option<crate::audio_source::AudioUnavailableCode>,
) -> roomler_ai_remote_control::models::Unavailable {
    use crate::audio_source::AudioUnavailableCode as Why;
    use roomler_ai_remote_control::models::Unavailable;
    match why {
        Some(Why::NoSoundServer) => Unavailable::new(
            "no-sound-server",
            "This host sends no sound: it has no Pulse client library (libpulse-simple), so the agent cannot reach its sound server",
        ),
        Some(Why::NoSession) => Unavailable::new(
            "no-session",
            "This host sends no sound: the agent runs as root without the session helper that records a session's sound",
        ),
        _ => Unavailable::new(
            "no-monitor",
            "This host sends no sound: session audio is switched off on it (ROOMLERD_AUDIO_SESSION=0), and no capture device is named for it (ROOMLERD_AUDIO_SOURCE)",
        ),
    }
}

/// FR-97 P2a — a hello's audio offer: the `audio` words and the
/// `unavailable["audio"]` reason, which travel together. A supervised Mac's
/// root daemon says its GUI worker's (`DelegateHost::effective_audio`): the
/// worker captures a delegated session's sound, and the root, in no
/// window-server session, could capture none of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioWords {
    pub audio: Vec<String>,
    pub why: Option<roomler_ai_remote_control::models::Unavailable>,
}

impl AudioWords {
    /// The offer `caps` makes.
    pub fn of(caps: &AgentCaps) -> Self {
        Self {
            audio: caps.audio.clone(),
            why: caps.unavailable.get("audio").cloned(),
        }
    }

    /// Make `caps` make this offer: the words, and the reason or none.
    pub fn apply(self, caps: &mut AgentCaps) {
        caps.audio = self.audio;
        match self.why {
            Some(why) => {
                caps.unavailable.insert("audio".into(), why);
            }
            None => {
                caps.unavailable.remove("audio");
            }
        }
    }
}

/// FR-97 P2a — a Mac's reason for sending no sound, by the refusal's code.
/// `None` reads as no window-server session: the only way a Mac with the
/// backend built has none and is not switched off.
fn mac_audio_word(
    why: Option<crate::audio_source::AudioUnavailableCode>,
) -> roomler_ai_remote_control::models::Unavailable {
    use crate::audio_source::AudioUnavailableCode as Why;
    use roomler_ai_remote_control::models::Unavailable;
    match why {
        Some(Why::SwitchedOff) => Unavailable::new(
            "switched-off",
            "This Mac sends no sound: system audio is switched off on it (ROOMLERD_SCK_AUDIO=0)",
        ),
        _ => Unavailable::new(
            "no-session",
            "This process has no window-server session (a Mac's root daemon), so it captures no sound; \
             the GUI worker in the person's login session does, and while it is attached this device \
             says what that worker can",
        ),
    }
}

/// FR-97 P2a — see [`HostFacts::mac_audio_refusal`]: the kill switch first,
/// then whether this process is in a window-server session at all.
fn mac_audio_refusal() -> Option<crate::audio_source::AudioUnavailableCode> {
    #[cfg(all(target_os = "macos", feature = "audio"))]
    {
        use crate::audio_source::AudioUnavailableCode as Why;
        if !crate::audio::mac_audio_enabled() {
            return Some(Why::SwitchedOff);
        }
        if !crate::tcc::has_gui_session() {
            return Some(Why::NoSession);
        }
        None
    }
    #[cfg(not(all(target_os = "macos", feature = "audio")))]
    {
        None
    }
}

/// A root Linux daemon (a system install) runs outside every desktop session:
/// with neither display variable it has no clipboard to reach, the same test
/// capture makes before it reports `no_display`. A per-user daemon, and every
/// other OS, keeps the compile-time answer. FR-93 P7 makes the root case work
/// through the session; until then the honest answer is no.
fn clipboard_reachable() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: geteuid has no preconditions and cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        let display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
        !root || display
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// FR-97 P0c — take out of `caps` what this host cannot serve, and say why in
/// [`AgentCaps::unavailable`]. A word in the hello is a promise; the viewer
/// shows these reasons instead of a control that fails silently (FR-97 §3.1).
/// The census (`scripts/fr97-caps-census.sh`) counts the result by OS.
pub(crate) fn with_honest_words(mut caps: AgentCaps, facts: &HostFacts) -> AgentCaps {
    use roomler_ai_remote_control::models::{OsKind, Unavailable};
    if facts.audio_built && !facts.audio_backend {
        // Without a backend the session would carry a silent track (R1).
        caps.audio.clear();
        let why = match facts.os {
            OsKind::Linux => linux_audio_word(facts.linux_audio_refusal),
            OsKind::Macos => mac_audio_word(facts.mac_audio_refusal),
            OsKind::Windows => Unavailable::new(
                "no-backend",
                "System-audio capture is not implemented on this OS yet, so this host sends no sound",
            ),
        };
        caps.unavailable.insert("audio".into(), why);
    }
    if !facts.clipboard_reachable && (caps.supports_clipboard || !caps.clipboard.is_empty()) {
        caps.supports_clipboard = false;
        caps.clipboard.clear();
        caps.unavailable.insert(
            "clipboard".into(),
            Unavailable::new(
                "no-session",
                "This daemon runs as root outside any desktop session, so it has no clipboard to reach",
            ),
        );
    } else if !facts.watches_clipboard_images
        && caps.clipboard.iter().any(|w| w == "events")
        && caps.clipboard.iter().any(|w| w == "images")
    {
        // `events` still pushes text; only copied images go unwatched (R8).
        caps.unavailable.insert(
            "clipboard-image-events".into(),
            Unavailable::new(
                "not-watched",
                "Copied images are not watched on this host, so they do not sync on their own: send or fetch one with the clipboard buttons",
            ),
        );
    }
    // FR-97 P3e — the layout chip and picker only where the layout can be
    // read and switched. No reason in `unavailable`: there is no control to
    // grey, the chip simply does not appear, as on a host before P3e.
    if !facts.layout_reachable {
        caps.layout.clear();
    } else if !facts.layout_switchable {
        // GNOME on Xorg: the chip reports; a picker would only snap back.
        caps.layout.retain(|w| w != "set");
    }
    // FR-97 P3e-2 — where display match cannot switch this host's display.
    // Until now the viewer offered the toggle by OS (Windows only), which hid
    // a Linux virtual desktop that could switch; it follows this word now.
    if !facts.display_switchable {
        let why = match facts.os {
            // FR-97 P2d-2b — a Mac switches unless its owner turned it off.
            OsKind::Macos => Unavailable::new(
                "unsupported",
                "This Mac's display cannot be switched to your window: display match is turned off on this Mac",
            ),
            OsKind::Linux | OsKind::Windows => Unavailable::new(
                "unsupported",
                "This host's display cannot be switched to your window: display match needs an X11 desktop with a per-user install, or the agent's own virtual desktop; a Wayland desktop and a system install keep their screen as it is",
            ),
        };
        caps.unavailable.insert("display-match".into(), why);
    }
    // The viewer's button types three ordinary keys everywhere.
    let sas = match facts.os {
        // A host that may send the real one (FR-97 P4a: SYSTEM, and a policy
        // that lets services) says so live, in the announcement, and drops
        // this entry; the rest keep it with the reason for theirs.
        OsKind::Windows => Unavailable::new(
            "keys-only",
            "Ctrl+Alt+Del is sent as ordinary keys: apps on the desktop get it, but the Windows lock and sign-in screens do not",
        ),
        // Not meaningless: the desktop binds the chord, and what it binds is
        // worth knowing before pressing it.
        OsKind::Linux => Unavailable::new(
            "keys-only",
            "Ctrl+Alt+Del is sent as ordinary keys, and the desktop decides what they do: GNOME and KDE bind them to log out",
        ),
        OsKind::Macos => Unavailable::new(
            Unavailable::NOT_APPLICABLE,
            "Ctrl+Alt+Del has no system meaning on macOS",
        ),
    };
    caps.unavailable.insert("sas".into(), sas);
    caps
}

/// The config-derived lists, assigned by the DAEMON over whatever
/// `cached_or_probed` answered — the caps-probe child's struct on a miss, the
/// merged cache on a hit, the driver-free fallback when the child died. All
/// three arrive here by this one line, and every list is OVERWRITTEN, never
/// merged: the child loads no config, so anything it carried in these fields
/// was computed against the built-in defaults.
///
/// - `rpc` — config-derived, not driver-probed (`relay_server_enabled` is an
///   opt-in), so it is computed where the config fallbacks are registered.
/// - `apps` — FR-56 AC10 (review of #1667): until it moved here,
///   `[virtual_desktop_apps] enabled = false` did not stop the hello
///   advertising `list` on every release's first boot, or on any boot of a
///   host with no hardware cell (never cached). And the child must not run a
///   privilege-dropped desktop walk.
/// - `files` — #1672, the same hole: `browse` follows `enable_remote_browse`,
///   and the child's flag is the built-in default (`true`). Set once at
///   startup and never live, so a boot-time snapshot here is exactly as fresh
///   as the flag itself.
///
/// A named seam rather than three lines inside the `OnceLock` closure so a
/// test can lock EACH assignment (`the_daemon_assigns_every_config_list`):
/// with the lines inline, deleting `caps.files = files_caps()` left every lib
/// test green while every hello shipped an empty `files` and every browser
/// fell back to upload-only — `detect()` itself cannot be called from a unit
/// test without spawning the probe child.
fn with_config_lists(mut caps: AgentCaps) -> AgentCaps {
    caps.rpc = rpc_caps();
    caps.apps = apps_caps();
    caps.files = files_caps();
    caps
}

/// FR-77 P3 — the probe cache in front of the child probe. A hit reuses the
/// last probe's encoder matrix (see `caps_cache` for exactly which fields);
/// a miss probes and, when the answer is worth keeping, stores it. Every
/// outcome is one log line, because "why did this host re-probe" is the
/// question the cache will be asked.
fn cached_or_probed() -> AgentCaps {
    use super::caps_cache as cache;

    let now = cache::now_unix();
    let enabled = cache::enabled();
    let key = if enabled { cache::current_key() } else { None };
    let path = cache::path();
    match (&key, &path) {
        (Some(key), Some(path)) => match cache::load_matching(path, key, now) {
            Ok(hit) => {
                tracing::info!(
                    path = %path.display(),
                    age_secs = now.saturating_sub(hit.probed_at_unix),
                    probe_ms = hit.probe_ms,
                    cells = hit.caps.video_cells.len(),
                    hardware = %key.hardware,
                    "caps probe: cache hit — reusing the last probe's encoder matrix"
                );
                if !hit.learned_deny.is_empty() {
                    tracing::warn!(
                        learned_deny = ?hit.learned_deny,
                        "caps probe: that probe learned to deny these cells — sessions will not \
                         open them either; a driver, build or knob change, or a restart after a \
                         day, tests them again"
                    );
                }
                super::cells::install_learned(hit.learned_deny.clone());
                install_vp9_qsv_idr(hit.vp9_qsv_idr);
                install_gpu_forms(hit.gpu_input.clone());
                return merge_cached(hit);
            }
            Err(miss) => tracing::info!(
                path = %path.display(),
                reason = %miss.reason(),
                "caps probe: cache miss — probing"
            ),
        },
        _ if !enabled => {
            tracing::info!("caps probe: cache disabled (ROOMLERD_CAPS_CACHE=0) — probing")
        }
        (Some(_), None) => {
            tracing::debug!("caps probe: no directory to cache in — probing")
        }
        _ => tracing::debug!("caps probe: no cache key on this platform — probing"),
    }

    let got = child::probe(&child::Try {
        bounds: child::Bounds::FIRST,
        denied: Vec::new(),
        stop: &|| false,
        first: true,
    });
    // Whatever the runs left, a session opens encoders in the daemon, and the
    // fault that ended the child would end the daemon.
    super::cells::install_learned(got.learned.clone());
    *FIRST_SHORT.lock().unwrap_or_else(|e| e.into_inner()) = got.short;
    match got.report {
        Some(report) => {
            install_vp9_qsv_idr(report.vp9_qsv_idr);
            install_gpu_forms(report.gpu_input.clone());
            if let (Some(key), Some(path)) = (key, path) {
                store_answer(key, &path, now, &report, &got.learned, got.unanswered);
            }
            report.caps
        }
        None => {
            // No child came back, the one that asks for no driver included
            // (`child::run_software`). "Codec unavailable" is the only honest
            // reading: we have no evidence this host can encode with any of
            // them, and advertising one we cannot produce costs a black
            // session. Everything that needs no driver still stands.
            compute_caps(false, false)
        }
    }
}

/// Keep a probe's answer for the next start, if [`why_not_cached`] lets it.
fn store_answer(
    key: super::caps_cache::CacheKey,
    path: &std::path::Path,
    now: u64,
    report: &child::ProbeReport,
    learned: &[super::cells::LearnedDeny],
    unanswered: bool,
) {
    use super::caps_cache as cache;
    if let Some(why) = why_not_cached(&report.caps, learned, unanswered) {
        tracing::info!(learned_deny = ?learned, "caps probe: not cached: {why}");
        return;
    }
    let file = cache::CacheFile::new(
        key,
        now,
        report.caps.probe_ms.unwrap_or(0),
        report.caps.clone(),
        report.vp9_qsv_idr,
    )
    .with_gpu_input(report.gpu_input.clone())
    .with_learned_deny(learned.to_vec());
    match cache::store(path, &file) {
        Ok(()) => tracing::info!(
            path = %path.display(),
            "caps probe: result cached for the next start"
        ),
        Err(e) => tracing::warn!(
            %e,
            path = %path.display(),
            "caps probe: could not write the cache — harmless, the next start probes again"
        ),
    }
}

/// Why this probe's answer must not outlive the process, or `None` when it may.
/// A later try's answer passes through here unchanged (#3280).
fn why_not_cached(
    caps: &AgentCaps,
    learned: &[super::cells::LearnedDeny],
    unanswered: bool,
) -> Option<&'static str> {
    if learned.iter().any(|l| !l.confirmed) {
        // #3267 — this answer leans on a cell a hang or a kill may have cost for
        // nothing. Freezing it would keep a healthy cell out for as long as the
        // cache lives.
        Some("a cell is denied on a hang or a kill only, so the next start tests it again")
    } else if unanswered {
        // #3273 — those forms are missing for want of an answer, and a hit would
        // not ask again for a week. Only a fault's death is an answer (#3280).
        Some("a 4:4:4 or GPU child left no answer, so the next start asks again")
    } else if !super::caps_cache::worth_caching(caps) {
        Some("no hardware cell opened (a boot-time driver race must not be frozen for a week)")
    } else {
        None
    }
}

/// A cache hit: the driver-derived fields from the cache, everything else
/// computed fresh by THIS process (permissions, the GUI session and the
/// clipboard verbs change without any driver changing). The config-derived
/// lists — `rpc`, `apps`, `files` — are neither cached nor computed here:
/// `detect()` assigns them after the merge, by the same line a miss takes.
fn merge_cached(hit: super::caps_cache::CacheFile) -> AgentCaps {
    let mut fresh = compute_caps(false, false);
    take_probed_fields(&mut fresh, hit.caps);
    fresh.probe_ms = Some(hit.probe_ms);
    fresh.probe_cached = true;
    fresh
}

/// What a probe answers, as against what the running process says for itself. A
/// cache hit and an adopted try move exactly these.
fn take_probed_fields(into: &mut AgentCaps, from: AgentCaps) {
    into.hw_encoders = from.hw_encoders;
    into.codecs = from.codecs;
    into.transports = from.transports;
    into.hevc_chroma = from.hevc_chroma;
    into.vp9_chroma = from.vp9_chroma;
    into.video_cells = from.video_cells;
}

/// #3280 — one row of the table a host short of time climbs: how long to wait
/// after the last try ended, and how long this one gives its children.
struct Rung {
    wait: Duration,
    bounds: child::Bounds,
}

const fn rung(wait_min: u64, child_min: u64, kill_wait_s: u64, budget_min: u64) -> Rung {
    Rung {
        wait: Duration::from_secs(wait_min * 60),
        bounds: child::Bounds {
            child: Duration::from_secs(child_min * 60),
            kill_wait: Duration::from_secs(kill_wait_s),
            base_budget: Duration::from_secs(budget_min * 60),
        },
    }
}

/// #3280 — the first probe blocks the device, so its bounds cannot grow. These do
/// not, so they can. Five tries over about a day and a half, and each is a whole
/// probe, because its answer has to stand alone.
const LADDER: [Rung; 5] = [
    rung(1, 2, 20, 3),
    rung(10, 4, 40, 6),
    rung(60, 8, 80, 12),
    rung(6 * 60, 8, 80, 12),
    rung(24 * 60, 8, 80, 12),
];

/// #3280 — how often the wait for a quiet host looks.
const RETRY_TICK: Duration = Duration::from_secs(1);
/// #3280 — no try starts this soon after a session asked. It may be waiting on a
/// person at the consent prompt, with nothing in the registry yet.
const QUIET_AFTER_ASK: Duration = Duration::from_secs(60);

/// #3280 — `ROOMLERD_CAPS_PROBE_RETRY=0` / `caps_probe_retry = false`: a first
/// answer short of time stands until the next start, as before. Read at each try.
fn retry_enabled() -> bool {
    tunnel_core::env::flag("CAPS_PROBE_RETRY", true)
}

/// #3280 — a session asked for this host. A try's child is told to die at its next
/// poll, because a try has no value until it finishes and a session sharing the GPU
/// with a slow or stuck child can wedge on the same driver. Cheap, and harmless
/// with no try running. Earlier than the registry can say: a session enters that
/// only after its peer is built and its pump has started.
pub fn session_asked() {
    *LAST_ASK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}

fn last_ask() -> Option<Instant> {
    *LAST_ASK.lock().unwrap_or_else(|e| e.into_inner())
}

/// #3280 — whether a try may start, or the wait for one may count. Not while a
/// child that would not die is still stuck: the driver has not recovered. Not
/// while a session is live, a recording that outlives one included, or just asked.
fn quiet(stuck: u32, live: bool, since_ask: Option<Duration>) -> bool {
    stuck == 0 && !live && since_ask.is_none_or(|t| t >= QUIET_AFTER_ASK)
}

/// #3280 — what a later try starts with denied: everything the daemon goes by,
/// except what is there only for want of time. A fault's deny and a double kill's
/// stay on every try's list.
fn list_for_a_try(list: &[super::cells::LearnedDeny]) -> Vec<super::cells::LearnedDeny> {
    list.iter().filter(|d| !d.timed_out).cloned().collect()
}

/// Every form an answer advertises, by the denylists' spellings.
fn forms_of(caps: &AgentCaps, gpu_input: &[String]) -> std::collections::BTreeSet<String> {
    let mut forms: std::collections::BTreeSet<String> = caps
        .video_cells
        .iter()
        .flat_map(|c| {
            c.chroma
                .iter()
                .map(move |chroma| format!("{}_{}:{chroma}", c.codec, c.backend))
        })
        .collect();
    forms.extend(gpu_input.iter().map(|name| format!("{name}:gpu")));
    forms
}

/// #3280 — what one try means for the next.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Next {
    /// A session stopped it, so it does not count: the same row again once
    /// nothing has been live or asked for a minute.
    Again,
    /// Still short of time: the next row, with what it was short by.
    Climb(child::Shortfall),
    End(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Verdict {
    adopt: bool,
    next: Next,
}

/// #3280 — whether a try's answer replaces the one in force, and what comes next.
/// Pure, so every row and every stop is a test with no child in it.
///
/// A try cannot take a form from a host, except for a fault: an answer is adopted
/// when it keeps everything in force and has no shortfall or has more, or when it
/// confirmed a new fault in a form in force, which no session can be left to find.
fn weigh(
    in_force: &AgentCaps,
    in_force_gpu: &[String],
    denied_before: &[super::cells::LearnedDeny],
    last: &child::Shortfall,
    got: &child::Probed,
) -> Verdict {
    if got.stopped {
        return Verdict {
            adopt: false,
            next: Next::Again,
        };
    }
    let adopt = got.report.as_ref().is_some_and(|report| {
        let had = forms_of(in_force, in_force_gpu);
        let has = forms_of(&report.caps, &report.gpu_input);
        let new_fault = got.learned.iter().any(|d| {
            d.confirmed && had.contains(&d.cell) && !denied_before.iter().any(|b| b.cell == d.cell)
        });
        new_fault || (had.is_subset(&has) && (got.short.is_none() || has.len() > had.len()))
    });
    let next = match &got.short {
        None if got.report.is_some() => Next::End("it answered"),
        None => Next::End("it left no answer, and not for want of time"),
        // The signature of a real hang, and another try could hold the GPU for
        // minutes on a host someone may be using. The cost: a host that needs more
        // than the first try's bound for one open loses that one form.
        Some(now) if now.cells.iter().any(|c| last.cells.contains(c)) => {
            Next::End("the same cell ran out of time again, with more of it")
        }
        Some(now) => Next::Climb(now.clone()),
    };
    Verdict { adopt, next }
}

/// #3280 — climb [`LADDER`] from the first answer's shortfall, one try at a time.
/// `wait` is told what the table owes and returns when that has passed quietly; a
/// try a session stopped owes nothing but quiet. Returns why it ended.
fn climb(
    first: child::Shortfall,
    mut wait: impl FnMut(Duration),
    mut attempt: impl FnMut(usize, &child::Shortfall) -> Next,
) -> &'static str {
    let mut last = first;
    let mut row = 0;
    let mut lost = false;
    while let Some(rung) = LADDER.get(row) {
        wait(if lost { Duration::ZERO } else { rung.wait });
        match attempt(row, &last) {
            Next::Again => lost = true,
            Next::Climb(short) => {
                last = short;
                row += 1;
                lost = false;
            }
            Next::End(why) => return why,
        }
    }
    "the end of the table"
}

/// #3280 — sit out `owed` of quiet, and at least one look.
fn wait_quietly(owed: Duration, live: &impl Fn() -> bool) {
    let mut owed = owed.max(RETRY_TICK);
    while !owed.is_zero() {
        std::thread::sleep(RETRY_TICK);
        let since_ask = last_ask().map(|t| t.elapsed());
        owed = after_a_look(owed, quiet(child::stuck_children(), live(), since_ask));
    }
}

/// #3280 — what is still owed after one look. Anything [`quiet`] says no to stops
/// the clock, and a try then owes a minute with nothing going on, however long
/// ago that session asked: it may only just have ended, and its viewer may be
/// reloading the page.
fn after_a_look(owed: Duration, quiet: bool) -> Duration {
    if quiet {
        owed.saturating_sub(RETRY_TICK)
    } else {
        owed.max(QUIET_AFTER_ASK)
    }
}

/// #3280 — one try: a whole probe on this rung's bounds and its own list. Its answer
/// goes in only if nothing went live or asked meanwhile, and the cache's gate is
/// the first probe's, with the key read before the probe started.
fn one_try(row: usize, last: &child::Shortfall, live: &impl Fn() -> bool) -> Next {
    use super::caps_cache as cache;
    if !retry_enabled() {
        return Next::End("ROOMLERD_CAPS_PROBE_RETRY is off");
    }
    let bounds = LADDER[row].bounds;
    // A session that asked before this read is caught here, and one after it by
    // `stop`, so none falls between the wait and the try.
    let asked = last_ask();
    if !quiet(child::stuck_children(), live(), asked.map(|t| t.elapsed())) {
        return Next::Again;
    }
    let stop = || last_ask() != asked || live();
    let now = cache::now_unix();
    let key = cache::enabled().then(cache::current_key).flatten();
    let path = cache::path();
    let denied = list_for_a_try(&super::cells::learned());
    let started = Instant::now();
    tracing::warn!(
        attempt = row + 1,
        child_bound_s = bounds.child.as_secs(),
        denied = ?denied.iter().map(|d| d.cell.as_str()).collect::<Vec<_>>(),
        "caps probe: a later try starts, with more time"
    );
    let mut got = child::probe(&child::Try {
        bounds,
        denied: denied.clone(),
        stop: &stop,
        first: false,
    });
    got.stopped |= stop();
    let in_force_gpu = GPU_IN_FORCE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let verdict = weigh(&detect(), &in_force_gpu, &denied, last, &got);
    tracing::warn!(
        attempt = row + 1,
        elapsed_ms = started.elapsed().as_millis(),
        stopped = got.stopped,
        short = ?got.short,
        cells = got.report.as_ref().map(|r| r.caps.video_cells.len()),
        adopt = verdict.adopt,
        next = ?verdict.next,
        "caps probe: a later try ended"
    );
    if verdict.adopt
        && let Some(report) = got.report
    {
        if let (Some(key), Some(path)) = (key, path) {
            store_answer(key, &path, now, &report, &got.learned, got.unanswered);
        }
        adopt(report, got.learned, got.short.as_ref());
    }
    verdict.next
}

/// #3280 — a later try's answer becomes the one in force. The list first, because
/// it decides what an open may do, and the generation last, so a heartbeat that
/// sees it finds the new answer.
fn adopt(
    report: child::ProbeReport,
    learned: Vec<super::cells::LearnedDeny>,
    short: Option<&child::Shortfall>,
) {
    super::cells::install_learned(learned.clone());
    install_vp9_qsv_idr(report.vp9_qsv_idr);
    install_gpu_forms(report.gpu_input.clone());
    let answer = adopted_answer(detect(), report.caps, &learned, short);
    tracing::warn!(
        hw_encoders = ?answer.hw_encoders,
        codecs = ?answer.codecs,
        cells = answer.video_cells.len(),
        learned_deny = ?learned.iter().map(|l| l.cell.as_str()).collect::<Vec<_>>(),
        "caps probe: a later try's answer is now the one in force, with no restart"
    );
    *LATE_CAPS.write().unwrap_or_else(|e| e.into_inner()) = Some(answer);
    CAPS_GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// The answer in force, with what a later try's probe answered. The config-derived
/// words stay as they are: they did not change, and re-deriving them here could
/// only move them for no reason. The word about the probe is replaced.
fn adopted_answer(
    mut in_force: AgentCaps,
    answer: AgentCaps,
    learned: &[super::cells::LearnedDeny],
    short: Option<&child::Shortfall>,
) -> AgentCaps {
    let probe_ms = answer.probe_ms;
    take_probed_fields(&mut in_force, answer);
    in_force.probe_ms = probe_ms;
    in_force.probe_cached = false;
    in_force.unavailable.remove(LEARNED_WORD);
    with_learned_word(in_force, learned, short)
}

/// #3280 — after the first answer, on a host whose first answer ran short of time:
/// another try by itself, with more time, with no restart, never blocking the start
/// or a live session. A cache hit or a complete answer arms nothing, so a healthy
/// host runs none of this. The first call of a process takes the shortfall, so a
/// second organization's loop arms nothing.
///
/// A thread of its own and not `spawn_blocking`: a runtime's shutdown waits on
/// those, and `signaling::run` must be able to leave. `live` says whether a session
/// is live on this device.
pub fn arm_retry(live: impl Fn() -> bool + Send + 'static) {
    let Some(short) = FIRST_SHORT.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    if !retry_enabled() {
        tracing::info!(
            ?short,
            "caps probe: the first answer ran short of time, and ROOMLERD_CAPS_PROBE_RETRY is \
             off: it stands until the next start"
        );
        return;
    }
    tracing::warn!(
        ?short,
        "caps probe: the first answer ran short of time — trying again with more, with no restart"
    );
    let spawned = std::thread::Builder::new()
        .name("roomlerd-caps-retry".into())
        .spawn(move || {
            let why = climb(
                short,
                |owed| wait_quietly(owed, &live),
                |row, last| one_try(row, last, &live),
            );
            tracing::info!(
                why,
                "caps probe: no more tries; the answer in force stands until the next start"
            );
        });
    if let Err(e) = spawned {
        tracing::warn!(%e, "caps probe: no thread for a later try; this answer stands until the next start");
    }
}

/// [`install_gpu_input`], and what [`weigh`] compares a later try against.
fn install_gpu_forms(proven: Vec<String>) {
    *GPU_IN_FORCE.lock().unwrap_or_else(|e| e.into_inner()) = proven.clone();
    install_gpu_input(proven);
}

#[cfg(feature = "ffmpeg-encoder")]
fn install_vp9_qsv_idr(verdict: Option<(bool, bool)>) {
    if let Some(v) = verdict {
        crate::encode::ffmpeg::FfmpegEncoder::set_vp9_qsv_idr_verdict(v);
    }
}

#[cfg(not(feature = "ffmpeg-encoder"))]
fn install_vp9_qsv_idr(_verdict: Option<(bool, bool)>) {}

/// FR-118 P2-3 — the GPU-input forms the probe proved, for the session's gate.
#[cfg(all(
    target_os = "windows",
    feature = "ffmpeg-encoder",
    any(feature = "mf-encoder", feature = "wgc-capture")
))]
fn install_gpu_input(proven: Vec<String>) {
    // FR-118 P3 — the capture side reads the same list to put its device on
    // those cells' adapter.
    crate::capture::gpu_frame::install_gpu_input_cells(proven.clone());
    crate::encode::ffmpeg::gpu_input::install_proven(proven);
}

/// FR-118 P5 — VideoToolbox's GPU forms the probe proved.
#[cfg(all(
    target_os = "macos",
    feature = "ffmpeg-encoder",
    feature = "scrap-capture"
))]
fn install_gpu_input(proven: Vec<String>) {
    crate::encode::ffmpeg::vt_input::install_proven(proven);
}

#[cfg(not(any(
    all(
        target_os = "windows",
        feature = "ffmpeg-encoder",
        any(feature = "mf-encoder", feature = "wgc-capture")
    ),
    all(
        target_os = "macos",
        feature = "ffmpeg-encoder",
        feature = "scrap-capture"
    )
)))]
fn install_gpu_input(_proven: Vec<String>) {}

#[cfg(feature = "ffmpeg-encoder")]
fn vp9_qsv_idr_verdict() -> Option<(bool, bool)> {
    crate::encode::ffmpeg::FfmpegEncoder::vp9_qsv_idr_verdict()
}

#[cfg(not(feature = "ffmpeg-encoder"))]
fn vp9_qsv_idr_verdict() -> Option<(bool, bool)> {
    None
}

/// The in-process probe as a report: what the `caps-probe` child computes
/// and prints, and what a process that IS the child answers directly.
/// `attempt_444` = false is the BASE phase (FR-77 P3c): every 4:2:0 open
/// and the software cells, with no FFmpeg 4:4:4 attempt — those run in the
/// second child so a faulting driver costs only the 4:4:4 form.
fn in_process_report(attempt_444: bool) -> child::ProbeReport {
    let caps = compute_caps(true, attempt_444);
    child::ProbeReport {
        caps,
        vp9_qsv_idr: vp9_qsv_idr_verdict(),
        // The GPU-input forms are proven by their own phase only.
        gpu_input: Vec::new(),
    }
}

/// #3267 — the `software` phase: everything that needs no driver call, and
/// libvpx's cell, which needs none either but does open an encoder, so it takes
/// a child to ask. No hardware cell, so never cached.
fn software_report() -> child::ProbeReport {
    child::ProbeReport {
        caps: compute_caps_with(false, false, true),
        vp9_qsv_idr: None,
        gpu_input: Vec::new(),
    }
}

/// #3267 — run one open, its teardown included, announced as `name:chroma`, so
/// a child that dies in it says which cell. `open` must leave nothing open
/// behind it: the announcement ends when it returns.
// A build with no encoder backend opens nothing.
#[allow(dead_code)]
fn announced_open<T>(name: &str, chroma: ChromaFormat, open: impl FnOnce() -> T) -> T {
    let _window = child::OpenWindow::begin(name, chroma);
    open()
}

/// Compute caps IN THIS PROCESS, running the hardware probes. This is what
/// the `caps-probe` child executes; nothing else should call it, because a
/// vendor driver that faults takes the whole process with it.
pub fn detect_in_process() -> AgentCaps {
    compute_caps(true, true)
}

/// Whether the OS will let this process capture the screen.
///
/// macOS is the only platform with a gate, and it is a silent one:
/// `CGDisplayStream` opens successfully without the Screen Recording grant
/// and delivers wallpaper-only frames forever. Probed rather than assumed so
/// the server can be told the truth. Cheap — a preflight call, no prompt.
/// Can this process reach a GUI session at all?
///
/// macOS's root LaunchDaemon cannot: session 0 has no WindowServer, so capture
/// and input are unavailable there no matter what TCC says. Every other
/// platform always can.
fn gui_session_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::tcc::has_gui_session()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

fn capture_permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::tcc::screen_recording_granted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// Whether the OS will let this process inject input.
///
/// Same shape as [`capture_permission_granted`]: macOS drops every
/// `CGEventPost` on the floor without the Accessibility grant and reports
/// success. Uses the TCC probe directly rather than constructing an injector,
/// which is lazy by design (first injected event) and has side effects.
fn input_permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::tcc::accessibility_trusted()
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

/// `AgentCaps.video`: properties of the video wire this build speaks.
/// `chunk-framing` always (FR-17); `bench` only on a bench target (FR-16 L4:
/// the synthetic source selected and `ROOMLERD_BENCH` on), so a viewer sends
/// its probes only where a frame will answer them. The probe child inherits
/// the environment, so it reads the same answer the daemon would.
fn video_features() -> Vec<String> {
    let mut features = vec!["chunk-framing".to_string()];
    #[cfg(feature = "synthetic-frame-source")]
    if crate::capture::synthetic_backend::bench_active() {
        features.push("bench".to_string());
    }
    // FR-119 P1 — the viewer may ask for more than 60 fps (`rc:fps`); the
    // answer rides `rc:video-info`. Off unless `ROOMLERD_FPS_HIGH=1`.
    if crate::encode::fps_high_enabled() {
        features.extend(crate::encode::frame_rate::CAPS.map(str::to_string));
    }
    // FR-130 P1 — this agent's encoders write the colour description they
    // converted with (`color_desc`), so a viewer knows before it connects.
    if crate::encode::color_description_enabled() {
        features.push(COLOR_DESCRIBED.to_string());
    }
    features
}

/// FR-130 P1 — the `AgentCaps.video` token for "every encoder that can carry a
/// colour description writes `color_desc::DESCRIPTION`". Absent with
/// `color_description` off and on every agent before P1.
pub const COLOR_DESCRIBED: &str = "color-described";

/// `run_hw_probes = false` computes everything that needs no driver call —
/// which is exactly the honest answer when the probe child did not come back.
fn compute_caps(run_hw_probes: bool, attempt_444: bool) -> AgentCaps {
    compute_caps_with(run_hw_probes, attempt_444, run_hw_probes)
}

/// [`compute_caps`], with libvpx's open apart from the hardware's (#3267): it
/// needs no driver, so a child that was told to touch none can still prove it
/// ([`software_report`]).
///
/// `unused_variables` is allowed because EVERY consumer of the flags sits
/// behind a `#[cfg]` (mf-encoder / ffmpeg-encoder / vp9-444); a default-feature
/// build probes nothing and legitimately never reads them.
#[allow(unused_variables)]
fn compute_caps_with(run_hw_probes: bool, attempt_444: bool, run_libvpx: bool) -> AgentCaps {
    // `mut` is only consumed inside the cfg-gated push blocks below
    // (openh264-encoder / mf-encoder). Default-feature builds skip
    // both blocks and the vecs stay empty; silence the unused-mut
    // lint to keep the CI `cargo clippy --workspace -- -D warnings`
    // build green on Linux.
    #[allow(unused_mut)]
    let mut codecs: Vec<String> = Vec::new();
    #[allow(unused_mut)]
    let mut hw_encoders: Vec<String> = Vec::new();
    // FR-77 — every cell (codec × chroma) this host OPENED, one entry per
    // encoder (codec × backend). Filled next to the legacy fields, which keep
    // their exact pre-FR-77 meaning; the cells carry the whole matrix.
    #[allow(unused_mut)]
    let mut video_cells: Vec<VideoCell> = Vec::new();

    #[cfg(feature = "openh264-encoder")]
    {
        codecs.push("h264".into());
        hw_encoders.push("openh264-sw".into());
        // Software by definition — needs no driver, so it is a cell even when
        // the hardware probes are not run.
        video_cells.push(VideoCell::new(
            VideoCodec::H264,
            VideoBackend::Openh264,
            &[ChromaFormat::Yuv420],
            false,
        ));
    }

    #[cfg(all(target_os = "windows", feature = "mf-encoder"))]
    if run_hw_probes {
        // #3267 — a denied MF cell is not opened, and its legacy label goes
        // with it (the H.264 label below is enumeration-gated, so an open that
        // is refused would not take it away).
        let mf_deny = denied_cells();
        // H.264: enumeration is sufficient. If any H.264 MFT
        // enumerates the cascade always succeeds (at worst it falls
        // through to the default-adapter SW MFT via
        // CLSID_MSH264EncoderMFT); runtime activation failure would
        // be caught by open_default's triple-fallback (MF → openh264
        // → Noop). No probe needed.
        if !cell_denied(&mf_deny, super::cells::MF_H264, ChromaFormat::Yuv420)
            && let Ok(adapters) = super::mf::probe_adapter_count()
            && adapters > 0
        {
            hw_encoders.push("mf-h264-hw".into());
            // FR-77 — the cell must say whether the cascade lands on
            // silicon, and only an activation knows (the legacy label above
            // is enumeration-only and says `-hw` even for the SW MFT).
            let probe = activates(CodecProbe::H264);
            if !matches!(probe, ProbeResult::Failed) {
                video_cells.push(VideoCell::new(
                    VideoCodec::H264,
                    VideoBackend::MediaFoundation,
                    &[ChromaFormat::Yuv420],
                    matches!(probe, ProbeResult::Hardware),
                ));
            }
        }

        let allow_sw = allow_sw_heavy_override();
        let advertise = |r: ProbeResult| -> bool {
            matches!(r, ProbeResult::Hardware)
                || (allow_sw && matches!(r, ProbeResult::SoftwareOnly))
        };

        // HEVC: enumeration + real activation probe. MFTs that
        // enumerate but fail ActivateObject (driver/adapter
        // mismatches, missing HEVC Video Extension) would poison a
        // negotiated session — the track is bound to video/HEVC
        // before the encoder opens, so failure means black video not
        // fallback-decode. Gate advertising on a successful HW probe;
        // SW-only paths are dropped so H.264 wins negotiation
        // (mediasoup-screenshare-grade quality on iGPU hosts).
        if !cell_denied(&mf_deny, super::cells::MF_HEVC, ChromaFormat::Yuv420)
            && let Ok(adapters) = super::mf::probe_hevc_adapter_count()
            && adapters > 0
        {
            let probe = activates(CodecProbe::Hevc);
            if advertise(probe) {
                codecs.push("h265".into());
                hw_encoders.push("mf-h265-hw".into());
                video_cells.push(VideoCell::new(
                    VideoCodec::Hevc,
                    VideoBackend::MediaFoundation,
                    &[ChromaFormat::Yuv420],
                    matches!(probe, ProbeResult::Hardware),
                ));
            }
        }

        // AV1: same reasoning as HEVC, with sharper impact — the
        // RTX 5090 Blackwell regression causes the NVIDIA AV1 MFT to
        // enumerate-and-fail on every activation on dev hardware
        // (see `Known Issues` in CLAUDE.md). Probe-at-startup
        // filters this out so the agent doesn't advertise a codec
        // it can't actually produce.
        if !cell_denied(&mf_deny, super::cells::MF_AV1, ChromaFormat::Yuv420)
            && let Ok(adapters) = super::mf::probe_av1_adapter_count()
            && adapters > 0
        {
            let probe = activates(CodecProbe::Av1);
            if advertise(probe) {
                codecs.push("av1".into());
                hw_encoders.push("mf-av1-hw".into());
                video_cells.push(VideoCell::new(
                    VideoCodec::Av1,
                    VideoBackend::MediaFoundation,
                    &[ChromaFormat::Yuv420],
                    matches!(probe, ProbeResult::Hardware),
                ));
            }
        }
    }

    #[allow(unused_mut)]
    let mut transports: Vec<String> = Vec::new();
    // P7 — chroma formats the HEVC DC transport can emit (see the probe's
    // Ok arm below + AgentCaps::hevc_chroma). Stays empty when the ffmpeg
    // HEVC probe fails or the feature is off.
    #[allow(unused_mut)]
    let mut hevc_chroma: Vec<String> = Vec::new();

    // rc.77 — FFmpeg HEVC over DataChannel.
    //
    // **DEFAULT ON** since rc.107 — `ffmpeg_backend_enabled()` is an explicit
    // opt-OUT (`ROOMLERD_USE_FFMPEG=0`), not the rc.77 opt-IN this comment
    // used to describe. The flip was forced in the field: an MSI
    // MajorUpgrade WIPES the service env block, so the dropped `=1`
    // silently disabled HEVC fleet-wide (black canvas, rc.105→rc.106).
    //
    // Advertisement is still gated on a REAL `FfmpegEncoder::new_hevc`
    // probe at the standard resolution, so a host without working HW
    // HEVC falls back cleanly regardless of the flag. On success
    // advertise both the `h265` codec (additive to whatever
    // MF found) and the `data-channel-hevc` transport. The browser's
    // rc:session.request can then ask for codec=h265 + transport=
    // data-channel-hevc and `peer.rs::media_pump` will route to the
    // HEVC DC pump.
    //
    // Pre-flight WebCodecs spike (2026-05-26) confirmed Chrome + Edge
    // decode Annex-B no-description HEVC. Gate 0 smoke (2026-05-29)
    // confirmed hevc_qsv works on Iris Xe Tiger Lake AND hevc_nvenc
    // works on RTX 5090 Blackwell — the two boxes MF was broken on.
    #[cfg(feature = "ffmpeg-encoder")]
    if run_hw_probes && crate::encode::ffmpeg::available() {
        use crate::encode::ffmpeg::FfmpegEncoder;

        // FR-77 — cells this build will not even TRY (the kill switch), and
        // whether a QSV open proves hardware on this build (see
        // `qsv_is_hardware_by_construction`).
        let deny = denied_cells();
        let qsv_hw = FfmpegEncoder::qsv_is_hardware_by_construction();
        if !deny.is_empty() {
            tracing::info!(denied = ?deny, "caps probe: cells on the denylist are not opened");
        }

        // rc.83 — probe vp9_qsv to surface in caps + heartbeat whether
        // this host can use Intel HW VP9. The transport advertisement
        // (`data-channel-vp9-444`) stays gated on the libvpx probe
        // above — both encoder paths emit the same VP9 bitstream that
        // the same browser worker decodes; only the encoder source
        // differs. The runtime peer.rs dispatch picks vp9_qsv at
        // session-establish time when this probe passed AND the host
        // didn't request 4:4:4 chroma (which vp9_qsv doesn't support).
        {
            let start_vp9 = std::time::Instant::now();
            // #3267 — `new_vp9`'s cascade, one announced open at a time, so a
            // child that dies here names the cell.
            let mut vp9: anyhow::Result<&'static str> =
                Err(anyhow::anyhow!("every VP9 4:2:0 cell is on the denylist"));
            for name in super::cells::names_420(VideoCodec::Vp9) {
                vp9 = announced_open(name, ChromaFormat::Yuv420, || {
                    FfmpegEncoder::new_named_probe(name, PROBE_WIDTH, PROBE_HEIGHT, false).map(drop)
                })
                .map(|()| name);
                if vp9.is_ok() {
                    break;
                }
            }
            match vp9 {
                Ok(name) => {
                    tracing::info!(
                        encoder = name,
                        elapsed_ms = start_vp9.elapsed().as_millis(),
                        "caps probe: ffmpeg VP9 (vp9_qsv) encoder activates — runtime peer dispatch will prefer it over libvpx SW on data-channel-vp9-444 sessions"
                    );
                    hw_encoders.push(format!("ffmpeg-{name}"));
                    // FR-77 — the VP9 cell. 4:4:4 (profile 1) is attempted
                    // when the FFmpeg source matrix allows it for this name
                    // and the cell is not denylisted; today the open asks for
                    // planar yuv444p, which vp9_qsv refuses (it wants packed
                    // VUYX) — an honest failure until P3 teaches the pump
                    // that format. The session pump keeps vp9_qsv 4:2:0-only
                    // regardless (`peer.rs`).
                    let mut chroma = vec![ChromaFormat::Yuv420];
                    if let Some((cell_codec, backend)) = VideoBackend::from_ffmpeg_name(name) {
                        if attempt_444
                            && ffmpeg_444_capable(name)
                            && !cell_denied(&deny, name, ChromaFormat::Yuv444)
                        {
                            match announced_open(name, ChromaFormat::Yuv444, || {
                                FfmpegEncoder::new_named_probe(
                                    name,
                                    PROBE_WIDTH,
                                    PROBE_HEIGHT,
                                    true,
                                )
                                .map(drop)
                            }) {
                                Ok(()) => chroma.push(ChromaFormat::Yuv444),
                                Err(e) => {
                                    tracing::debug!(encoder = name, %e, "caps probe: 4:4:4 cell did not open")
                                }
                            }
                        }
                        video_cells.push(VideoCell::new(cell_codec, backend, &chroma, qsv_hw));
                    }
                    // P4 — measure whether THIS host's vp9_qsv honours
                    // runtime forced IDRs, per low_power mode. Hosts that
                    // do get GOP 64800 + on-demand-only keys (kills the
                    // residual ~1 Hz natural-key pulse on VP9-over-DC);
                    // hosts that don't keep the rc.219 containment. Escape
                    // hatch ROOMLERD_VP9_QSV_IDR_PROBE=0.
                    if tunnel_core::env::node_env("VP9_QSV_IDR_PROBE").as_deref() == Some("0") {
                        tracing::info!(
                            "caps probe: vp9_qsv IDR probe disabled (ROOMLERD_VP9_QSV_IDR_PROBE=0) — keeping GOP-60 containment"
                        );
                    } else if cell_denied(&deny, "vp9_qsv", ChromaFormat::Yuv420) {
                        // #3267 — it names `vp9_qsv` itself, whichever VP9 cell
                        // won above, and a denied cell is never opened.
                        tracing::info!(
                            "caps probe: vp9_qsv:yuv420 is on the denylist — no IDR probe"
                        );
                    } else {
                        let t_idr = std::time::Instant::now();
                        if let Some((lp1, lp0)) =
                            announced_open("vp9_qsv", ChromaFormat::Yuv420, || {
                                FfmpegEncoder::probe_and_cache_vp9_qsv_idr()
                            })
                        {
                            tracing::info!(
                                honors_low_power = lp1,
                                honors_vme = lp0,
                                elapsed_ms = t_idr.elapsed().as_millis(),
                                "caps probe: vp9_qsv runtime-IDR verdict (either true → long GOP + on-demand keys; both false → GOP-60 containment)"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::info!(
                        %e,
                        elapsed_ms = start_vp9.elapsed().as_millis(),
                        "caps probe: no ffmpeg VP9 hardware encoder opened (vp9_qsv, vp9_vaapi: NVIDIA host, Intel without QSV, no VAAPI device, or a driver issue) — VP9 sessions stay on libvpx SW"
                    );
                }
            }
        }

        // FR-77 — the MATRIX pass for HEVC / AV1 / H.264. Every backend in
        // cascade order is opened (not just the first that works) so
        // `video_cells` carries the host's whole encoder × chroma matrix, and
        // every winner the FFmpeg source matrix says can do 4:4:4 is re-opened
        // in that format — asked for, never asserted by name (P7 used to
        // advertise HEVC 4:4:4 for `hevc_nvenc` without opening it). The
        // legacy fields (`codecs`, `transports`, `hw_encoders`, `hevc_chroma`)
        // keep their exact pre-FR-77 meaning: the FIRST backend in cascade
        // order that opens, because a session still cascades in that order
        // and a viewer older than FR-77 reads nothing else.
        //
        // P2 (Parsec-class plan) — H.264 over DataChannel joins the reliable-
        // DC + WebCodecs + canvas pipeline (the RTP track + <video> path stays
        // as the universal fallback). The bitstream is Annex-B with in-band
        // SPS/PPS — the contract the HEVC path ships. `ROOMLERD_DC_H264=0`
        // stops that advertisement without a rebuild; the H.264 cells are
        // then not probed either (a cell nobody can negotiate is noise).
        let dc_h264 = tunnel_core::env::node_env("DC_H264").as_deref() != Some("0");
        for codec in [VideoCodec::Hevc, VideoCodec::Av1, VideoCodec::H264] {
            if codec == VideoCodec::H264 && !dc_h264 {
                tracing::info!(
                    "caps probe: data-channel-h264 advertisement disabled (ROOMLERD_DC_H264=0)"
                );
                continue;
            }
            let start = std::time::Instant::now();
            let mut winner: Option<&'static str> = None;
            let mut winner_has_444 = false;
            for name in FfmpegEncoder::cascade_names(codec) {
                // A table name outside the vocabulary would open and then
                // advertise nothing; `every_cascade_name_is_in_the_cell_vocabulary`
                // makes that a test failure rather than a silent hole.
                let Some((cell_codec, backend)) = VideoBackend::from_ffmpeg_name(name) else {
                    continue;
                };
                // FR-77 P4 — the denylist is the kill switch for BOTH chroma
                // forms. Until this arm it gated only the 4:4:4 open, so
                // `hevc_vaapi:yuv420` in `encoder_cells_deny` still opened the
                // cell (measured on jupiter): an operator's one lever against
                // a driver that faults on open did nothing for 4:2:0.
                if cell_denied(&deny, name, ChromaFormat::Yuv420) {
                    tracing::info!(
                        encoder = name,
                        "caps probe: 4:2:0 cell on the denylist — not opened, not advertised"
                    );
                    continue;
                }
                let t = std::time::Instant::now();
                match announced_open(name, ChromaFormat::Yuv420, || {
                    FfmpegEncoder::new_named_probe(name, PROBE_WIDTH, PROBE_HEIGHT, false).map(drop)
                }) {
                    Ok(()) => {
                        let mut chroma = vec![ChromaFormat::Yuv420];
                        if attempt_444
                            && ffmpeg_444_capable(name)
                            && !cell_denied(&deny, name, ChromaFormat::Yuv444)
                        {
                            match announced_open(name, ChromaFormat::Yuv444, || {
                                FfmpegEncoder::new_named_probe(
                                    name,
                                    PROBE_WIDTH,
                                    PROBE_HEIGHT,
                                    true,
                                )
                                .map(drop)
                            }) {
                                Ok(()) => chroma.push(ChromaFormat::Yuv444),
                                Err(e) => tracing::info!(
                                    encoder = name,
                                    %e,
                                    "caps probe: 4:4:4 cell did not open — advertising 4:2:0 only"
                                ),
                            }
                        }
                        let hw = match backend {
                            VideoBackend::Qsv => qsv_hw,
                            _ => true,
                        };
                        tracing::info!(
                            encoder = name,
                            chroma = ?chroma.iter().map(|c| c.wire()).collect::<Vec<_>>(),
                            hw,
                            elapsed_ms = t.elapsed().as_millis(),
                            "caps probe: ffmpeg encoder cell opened"
                        );
                        if winner.is_none() {
                            winner = Some(name);
                            winner_has_444 = chroma.contains(&ChromaFormat::Yuv444);
                        }
                        video_cells.push(VideoCell::new(cell_codec, backend, &chroma, hw));
                    }
                    Err(e) => {
                        // A backend the host lacks failing is the matrix doing
                        // its job — DEBUG, like the cascade's own candidates.
                        tracing::debug!(encoder = name, %e, "caps probe: ffmpeg encoder cell did not open");
                    }
                }
            }
            let elapsed_ms = start.elapsed().as_millis();
            match (codec, winner) {
                (VideoCodec::Hevc, Some(name)) => {
                    tracing::info!(
                        encoder = name,
                        elapsed_ms,
                        "caps probe: ffmpeg HEVC encoder activates — advertising h265 + data-channel-hevc"
                    );
                    if !codecs.iter().any(|c| c == "h265") {
                        codecs.push("h265".into());
                    }
                    transports.push("data-channel-hevc".into());
                    hw_encoders.push(format!("ffmpeg-{name}"));
                    // P7 — the session's 4:4:4 (Rext) path tries `hevc_nvenc`
                    // only, so the legacy field says 4:4:4 only when THAT
                    // backend won AND its 4:4:4 open succeeded just now.
                    hevc_chroma.push("yuv420".into());
                    if name == "hevc_nvenc" && winner_has_444 {
                        hevc_chroma.push("yuv444".into());
                    }
                }
                (VideoCodec::Hevc, None) => tracing::warn!(
                    elapsed_ms,
                    "caps probe: ffmpeg HEVC encoder failed to init — NOT advertising data-channel-hevc"
                ),
                (VideoCodec::Av1, Some(name)) => {
                    tracing::info!(
                        encoder = name,
                        elapsed_ms,
                        "caps probe: ffmpeg AV1 encoder activates — advertising av1 + data-channel-av1"
                    );
                    if !codecs.iter().any(|c| c == "av1") {
                        codecs.push("av1".into());
                    }
                    transports.push("data-channel-av1".into());
                    hw_encoders.push(format!("ffmpeg-{name}"));
                }
                (VideoCodec::Av1, None) => tracing::info!(
                    elapsed_ms,
                    "caps probe: ffmpeg AV1 encoder not available (no AV1 encode silicon on this host) — NOT advertising data-channel-av1"
                ),
                (VideoCodec::H264, Some(name)) => {
                    tracing::info!(
                        encoder = name,
                        elapsed_ms,
                        "caps probe: ffmpeg H.264 encoder activates — advertising data-channel-h264"
                    );
                    transports.push("data-channel-h264".into());
                    hw_encoders.push(format!("ffmpeg-{name}"));
                }
                (VideoCodec::H264, None) => tracing::info!(
                    elapsed_ms,
                    "caps probe: ffmpeg H.264 HW encoder not available — H.264 sessions stay on the RTP track"
                ),
                (VideoCodec::Vp9, _) => {}
            }
        }
    }

    #[cfg(feature = "vp9-444")]
    if run_libvpx {
        // Phase Y.4 caps probe (Y.runtime-encoder rewrite landed,
        // 0.1.47). Try to instantiate the libvpx encoder at a probe
        // resolution; on success advertise both the transport and
        // the encoder label, on failure stay silent so no session
        // ever negotiates onto a broken path. The probe runs once
        // per agent process via the OnceLock cache.
        let start = std::time::Instant::now();
        match announced_open(super::cells::LIBVPX_VP9, ChromaFormat::Yuv444, || {
            crate::encode::libvpx::Vp9Encoder::new(PROBE_WIDTH, PROBE_HEIGHT).map(drop)
        }) {
            Ok(()) => {
                tracing::info!(
                    elapsed_ms = start.elapsed().as_millis(),
                    "caps probe: vp9-444 libvpx encoder activates — advertising data-channel-vp9-444 transport"
                );
                transports.push("data-channel-vp9-444".into());
                hw_encoders.push("libvpx-vp9-444-sw".into());
                // FR-77 — the libvpx cell: profile 0 (4:2:0) is opened for
                // real too, so the cell claims only what opened.
                let mut chroma = Vec::new();
                match announced_open(super::cells::LIBVPX_VP9, ChromaFormat::Yuv420, || {
                    crate::encode::libvpx::Vp9Encoder::new_with_fps_chroma(
                        PROBE_WIDTH,
                        PROBE_HEIGHT,
                        30,
                        crate::encode::libvpx::Vp9Chroma::Yuv420,
                    )
                    .map(drop)
                }) {
                    Ok(()) => chroma.push(ChromaFormat::Yuv420),
                    Err(e) => tracing::debug!(%e, "caps probe: libvpx 4:2:0 cell did not open"),
                }
                chroma.push(ChromaFormat::Yuv444);
                video_cells.push(VideoCell::new(
                    VideoCodec::Vp9,
                    VideoBackend::Libvpx,
                    &chroma,
                    false,
                ));
            }
            Err(e) => {
                tracing::warn!(
                    %e,
                    elapsed_ms = start.elapsed().as_millis(),
                    "caps probe: vp9-444 libvpx encoder failed to init — NOT advertising"
                );
            }
        }
    }

    // File-DC verbs: deliberately NOT computed here. `detect()` assigns them
    // from `files_caps()` in the daemon — `browse` follows a config flag the
    // caps-probe child never loads (#1672); see that function.

    // rc.61 — surface VP9 chroma format in caps so the browser worker
    // picks the right codec string for VideoDecoder.configure(). Empty
    // when vp9-444 transport isn't advertised (so we don't lie to the
    // client about a format we don't emit).
    let vp9_chroma: String = if transports.iter().any(|t| t == "data-channel-vp9-444") {
        #[cfg(feature = "vp9-444")]
        {
            crate::encode::libvpx::vp9_chroma_from_env()
                .as_str()
                .to_string()
        }
        #[cfg(not(feature = "vp9-444"))]
        {
            String::new()
        }
    } else {
        String::new()
    };

    // Audio track advertisement. Only when the `audio` feature is
    // compiled in do we offer a WebRTC Opus track (system/desktop
    // audio, opt-in per session). The browser reads this list to decide
    // whether to set `audio_enabled` in `rc:session.request`; an empty
    // list means the agent won't add an audio track at all. Mirrors the
    // vp9-444 pattern (advertise a capability only when the code path
    // that fulfils it is actually built). No runtime probe: a failed
    // audio-device open leaves the track silent and tells the controller
    // why (`rc:audio-unavailable`, FR-97 P3c-1). What the OS cannot
    // capture at all is taken out by `with_honest_words` (FR-97 P0c).
    #[allow(unused_mut)]
    let mut audio: Vec<String> = Vec::new();
    #[cfg(feature = "audio")]
    {
        audio.push("opus".into());
    }

    // Remote Apps verbs: deliberately NOT computed here. `detect()` assigns
    // them from `apps_caps()` in the daemon — see that function for why the
    // caps-probe child must never be the one to answer.

    // Clipboard protocol-v2 flags. Advertised purely on feature
    // presence (like `supports_clipboard`): the handlers for ack /
    // subscribe / image framing are compiled in with the `clipboard`
    // feature and degrade at runtime (a headless host without a
    // clipboard no-ops the DC exactly as v1 did). The browser gates
    // its auto-sync engine + image paths on these values.
    let clipboard: Vec<String> = if cfg!(feature = "clipboard") {
        let mut c: Vec<String> = vec![
            "ack".into(),
            "events".into(),
            "images".into(),
            // v2.1 — html lane: formatted text + web-hosted images
            // survive the round-trip (CF_HTML / text/html both ways).
            "html".into(),
        ];
        // v2.2 — native lane (RTF with embedded images). Windows-only:
        // the RTF read/write rides clipboard-win's raw format API.
        if cfg!(target_os = "windows") {
            c.push("native".into());
        }
        c
    } else {
        Vec::new()
    };

    // Keyboard-layout integration (rc.227) — status reporting +
    // manual set ride the control DC: Windows code in `input::layout`
    // behind enigo-input, (FR-97 P3e) X11's in `input::layout_x11`,
    // which `with_honest_words` takes out where there is no X11 session,
    // and (P2d-2a) a Mac's in `input::layout_mac`, out where the build
    // runs no AppKit main loop. The browser gates its layout chip + picker
    // on these.
    let layout: Vec<String> = if cfg!(all(
        any(
            target_os = "windows",
            target_os = "linux",
            target_os = "macos"
        ),
        feature = "enigo-input"
    )) {
        vec!["report".into(), "set".into()]
    } else {
        Vec::new()
    };

    // What the OS has actually granted, not what we were compiled with.
    // See `AgentCaps::permissions`: `None` (a pre-rc.454 agent) and
    // `Some([])` mean opposite things, so this is always `Some` here.
    let mut permissions: Vec<String> = Vec::new();
    if !gui_session_available() {
        // THIRD state, distinct from granted and denied: this process is not
        // in a GUI login session (macOS's root LaunchDaemon), so capture and
        // input are impossible regardless of any grant. Without saying so, a
        // mesh-only daemon reports "holds neither permission" and the device
        // list tells the operator to go fix something that is not broken.
        permissions.push("no-gui-session".into());
    } else {
        if capture_permission_granted() {
            permissions.push("screen-capture".into());
        }
        if input_permission_granted() {
            permissions.push("input".into());
        }
    }

    AgentCaps {
        hw_encoders,
        codecs,
        // Was `cfg!(feature = "enigo-input")` — a COMPILE-TIME constant, so a
        // Mac with Accessibility denied still advertised working input while
        // silently dropping every event. The feature must be compiled in AND
        // the OS must have granted it.
        has_input_permission: cfg!(feature = "enigo-input")
            && gui_session_available()
            && input_permission_granted(),
        permissions: Some(permissions),
        supports_clipboard: cfg!(feature = "clipboard"),
        supports_file_transfer: true,
        max_simultaneous_sessions: rc_max_sessions(),
        transports,
        // FR-17 — this build frames every DataChannel message, so a
        // receiver can reassemble without relying on the channel being
        // ordered. Advertised unconditionally: it is a property of the
        // code, not of the host or its hardware.
        video: video_features(),
        // Empty in the child's struct BY DESIGN; `detect()` fills it in the
        // daemon (`files_caps`). A test locks this.
        files: Vec::new(),
        vp9_chroma,
        hevc_chroma,
        audio,
        // Empty in the child's struct BY DESIGN; `detect()` fills it in the
        // daemon (`apps_caps`). A test locks this.
        apps: Vec::new(),
        clipboard,
        layout,
        video_cells,
        // Stamped by the PARENT after the child reports (`child::probe`);
        // the driver-free fallback has no probe to time.
        probe_ms: None,
        probe_cached: false,
        // P6 — the InputArbiter runs on every build (injection degrades to
        // Noop without enigo-input, but the arbitration/floor semantics
        // hold), so the server can safely lift the P3 single-INPUT-holder
        // downgrade for this agent.
        input: vec!["arbiter".into(), "exclusive".into(), "ghost-cursor".into()],
        // Multi-org — this agent honours `rc:agent.join_org` (enroll into an
        // additional org from a pushed token, no restart). Build-independent:
        // the enroll + config paths are in every flavour.
        multi_org: vec!["join".into()],
        // FR-67 P7 — `None` HERE on purpose: whether an update is acted on
        // depends on the connection (primary or secondary org) and on the
        // daemon's updater, neither of which the probe child knows.
        // `signaling::stub_caps` fills it at every announcement.
        update_mode: None,
        // Fleet RPC — build-independent (the exec engine is std process
        // spawning, no feature-gated backend). Advertising the capability
        // says only "this agent understands the verbs": the org kill-switch,
        // the device's ExecPolicy, and the agent's own `exec_enabled` config
        // key all still have to say yes before anything runs.
        rpc: rpc_caps(),
        // FR-85 P3b — empty HERE on purpose: `detect()` is memoized and
        // config-blind, while the record words follow the owner's LIVE gate.
        // `signaling::stub_caps` fills them at every announcement (the hello,
        // and a heartbeat that re-announces when they change).
        record: Vec::new(),
        // FR-93 P2b — `None` HERE on purpose, like `record`: the desktop brief
        // is live (a login or a logout changes it) and reads logind, which has
        // no business in the probe child. `stub_caps` fills it from the
        // daemon's own refresher at every announcement.
        desktop: None,
        // FR-101 P2b — `None` HERE for the same reason: the owner's floor is
        // live config, and `stub_caps` fills it at every announcement.
        session_policy: None,
        // FR-97 P0c — empty HERE: the honest "no" is the daemon's
        // (`with_honest_words`), like the config lists above.
        unavailable: Default::default(),
    }
}

/// FR-56 — the Remote Apps verbs, assigned by [`detect`] in the DAEMON and
/// never computed inside `compute_caps`.
///
/// Two reasons, one measured in the review of #1667. On a caps-cache MISS the
/// hello takes the caps-probe child's struct wholesale, and the child loads
/// no config — so an apps block inside `compute_caps` advertised `list` on a
/// device whose owner had set `[virtual_desktop_apps] enabled = false`, on
/// every release's first boot and on every boot of a host with no hardware
/// cell (a no-hardware answer is never cached). And the child exists to
/// contain untrusted driver code; a privilege-dropped `loginctl`/`wmctrl`
/// walk of somebody's login session has no business in it.
///
/// `list` / `focus` / `launch` are advertised only when this process can
/// manage a desktop right now (a Linux virtual desktop or a logged-in
/// X11/Xwayland session, Windows, or a macOS process in a window-server
/// session) AND the operator hasn't disabled it; the browser gates its Apps
/// menu on this list, and older agents omit the field. A supervised Mac's
/// root daemon is never in a session: its hello carries its GUI worker's
/// words instead (`DelegateHost::effective_apps`).
/// `status` (AC10) says "this build has a Remote Apps backend and answers
/// `rc:apps.list` honestly": with the list when there is a desktop, with
/// `unavailable: {code, reason}` when there is not. It is what lets the viewer
/// show WHY instead of hiding the button — and it matters because this struct
/// is a boot-time snapshot (`detect` memoizes it), so `list` absent here can
/// simply mean nobody had logged in yet when the daemon started. The reply is
/// live; the hello is not.
fn apps_caps() -> Vec<String> {
    apps_caps_for(&crate::apps::apps_config())
}

/// [`apps_caps`] against an explicit config — testable without installing the
/// process-global one, and the same seam the reply path answers through.
fn apps_caps_for(cfg: &crate::apps::VirtualDesktopAppsConfig) -> Vec<String> {
    let mut apps: Vec<String> = Vec::new();
    if crate::apps::has_backend() {
        apps.push("status".into());
    }
    if crate::apps::availability_for(cfg).is_ok() {
        apps.push("list".into());
        apps.push("focus".into());
        apps.push("launch".into());
    }
    apps
}

/// #1672 — the file-DC verbs, assigned by [`detect`] in the DAEMON and never
/// computed inside `compute_caps`.
///
/// The same hole #1667 closed for `apps`. On a caps-cache MISS the hello takes
/// the caps-probe child's struct wholesale, and the child loads no config, so
/// a files block inside `compute_caps` read `REMOTE_BROWSE_ENABLED` at its
/// built-in default (`true`) and advertised `browse` on a device whose owner
/// had set `enable_remote_browse = false` (or `ROOMLERD_DISABLE_BROWSE`) — on
/// every release's first boot and on every boot of a host with no hardware
/// cell (a no-hardware answer is never cached). The `files:dir` handler still
/// refused, so the controller drew a Browse affordance that failed when used:
/// a false capability, not an access hole.
///
/// `upload` / `download` / `download-folder` are always built into this agent;
/// `resume` (rc.19) says the agent stages uploads under
/// `<dest_dir>/.roomler-partial/<id>/` and can resume a mid-flight transfer
/// after a DC drop, and a browser that does not see it keeps the rc.18
/// fail-fast path. `browse` alone follows the config. Old browsers that see
/// an empty `files` list fall back to `supports_file_transfer` (upload-only),
/// which is why the daemon must fill the list and not merely append to it.
///
/// A boot-time snapshot in `detect()` is the honest home: the flag is stored
/// once by `main.rs::run_cmd` before the signalling loop starts and is not
/// live (`config_surface.rs` says so; no LocalAPI `ConfigSet` re-seeds it) —
/// unlike `record`, whose gate IS live and is therefore filled at every
/// announcement by `signaling::stub_caps`.
fn files_caps() -> Vec<String> {
    files_caps_for(crate::files::is_remote_browse_enabled())
}

/// [`files_caps`] against an explicit flag — testable without touching the
/// process-global one (a test that flipped it would leak into every other
/// test in the process).
fn files_caps_for(browse_enabled: bool) -> Vec<String> {
    let mut files = vec![
        "upload".to_string(),
        "download".to_string(),
        "download-folder".to_string(),
        "resume".to_string(),
    ];
    if browse_enabled {
        files.push("browse".to_string());
    }
    files
}

/// Fleet-RPC + roomler-SSH verb capabilities.
///
/// `exec` / `originate` are build-independent (the exec engine is std process
/// spawning). `ssh` is NOT: it is gated on the `ssh-server` feature, because
/// `rc:ssh.grant` reaching a build without the server would be recorded by
/// nobody and the caller would hang against a port that answers nothing — the
/// exact failure the capability list exists to prevent.
///
/// Advertising a verb says only "this agent understands it". The org
/// kill-switch, the device's policy and the agent-local config key all still
/// have to say yes before anything happens.
fn rpc_caps() -> Vec<String> {
    use roomler_ai_remote_control::models::RpcCap;

    // Named by VARIANT, not by string literal: the wire spelling lives in
    // exactly one place (`RpcCap::wire`) that both this producer and every
    // server-side consumer go through, so the two can no longer drift.
    // `Config` is unconditional: understanding the frame is a property of the
    // BUILD, not of any feature or local setting. Whether the device obeys it
    // is a separate question answered by `remote_config_enabled` and by
    // whether the frame arrived on the primary org's socket — advertising
    // here must not imply either, or the server would read "will comply" from
    // a verb that only means "will parse".
    // `ConfigReport` rides alongside `Config` in THIS build, but it is a
    // separate verb because rc.457/rc.458 shipped `config` alone: those agents
    // apply a pushed config and never say a word about it. A server that read
    // "reports back" out of `config` would wait forever for an answer from
    // most of the fleet — the identical trap `ssh` / `ssh-consent` exists to
    // avoid, recurring exactly as that doc predicted it would.
    // FR-52 — `external-access` is unconditional for the same reason `Config`
    // is: it says this build UNDERSTANDS cross-org access, which for the
    // server means one specific thing — that a consent prompt raised by this
    // agent can tell the person at the machine the controller is from OUTSIDE
    // their organization. That is why the server gates the ADMIN APPROVAL on
    // the verb and not just the session: approving a device that cannot say it
    // would put a promise on screen the device does not keep, and consent
    // given under it would not be informed consent.
    //
    // ⚠️ It says NOTHING about `external_access_enabled` (gate 3). Deriving
    // the verb from the opt-in would collapse "too old to be approved" and
    // "approved but opted out" into one state, and those have different fixes
    // — the first needs an agent update, the second a line in the device's own
    // config. The fleet view renders them apart, which it can only do because
    // these two facts arrive separately.
    let mut caps = vec![
        RpcCap::Exec,
        RpcCap::Originate,
        RpcCap::Config,
        RpcCap::ConfigReport,
        RpcCap::ExternalAccess,
    ];
    // FR-97 P5a — a Linux or macOS build runs `console_user` as the person at
    // its screen: SSH's account mode, and the `run_as` of `rc:rpc.exec`,
    // which the server sends only to a connection advertising this. A
    // property of the BUILD, like `exec`: whether anyone is at the screen is
    // asked at each command. Windows' `console_user` is its service's.
    #[cfg(unix)]
    caps.push(RpcCap::ConsoleUser);
    // FR-121 — this device reports its loopback relay to the server while the
    // relay is switched on, so a viewer on it lists it as "this browser runs
    // on". A device without an overlay yet still says it and reports `None`.
    if crate::rc_local_turn::enabled() {
        caps.push(RpcCap::LocalRelay);
    }
    // FR-128 — it decides `rc_overlay = auto` for each session, while that is
    // switched on. The hub sends `auto` only to a device that says this, so a
    // device with the decision switched off reads the policy as `off`.
    if crate::rc_overlay::auto_enabled() {
        caps.push(RpcCap::RcOverlayAuto);
    }
    if cfg!(feature = "ssh-server") {
        caps.push(RpcCap::Ssh);
        // P5d. Distinct from `ssh` because agents rc.419 and earlier advertise
        // `ssh` while silently ignoring `SshPolicy.consent_mode` — the server
        // refuses to store a non-auto consent policy for a device that cannot
        // honour it, rather than hand an admin a rule that reads as enforced
        // and isn't.
        caps.push(RpcCap::SshConsent);
        // FR-83. This build answers every `rc:ssh.grant` with
        // `rc:ssh.grant_ack` once the grant is redeemable, so the server may
        // hold the caller for it. A property of the BUILD, like `ssh`: the
        // ack is sent even when the device refuses — that is the point.
        caps.push(RpcCap::SshGrantAck);
        // FR-90 P3-8-2a. This build's SSH server holds an agent session's
        // grant (`one_command`) to ONE `exec` and nothing else. Distinct from
        // `ssh`, its prefix, because every build before it would read past
        // the field and serve that session's key a shell: the server sends a
        // session's grant only to a device that says this. A property of the
        // BUILD; the SYSTEM service says it for itself, when it publishes.
        caps.push(RpcCap::SshOneCommand);
    }
    // FR-52 P4 — this build can ADMIT an external session: bind it to a
    // verified login, check its offer's MAC, seal its answer. The server opens
    // an external session only on a device that says so, because one that
    // cannot would ignore `Request.external` and serve an outsider as an
    // ordinary controller — the password proven but never bound to anything.
    // A property of the BUILD, like `external-access`: gate 3 is the device's
    // own, and is read when the request arrives.
    if cfg!(feature = "external-access") {
        caps.push(RpcCap::ExternalSession);
    }
    // FR-100 P1b — this build signs its session answer with its device key
    // when the offer asks (a nonce). A property of the BUILD, in every build:
    // an enrollment that could not keep a key says so with an empty
    // `peer_auth_key` on its hello, and signs nothing.
    caps.push(RpcCap::PeerAuth);
    // FR-19 — advertise the org-relay SERVER only when this device has opted
    // in (`relay_server_enabled`, gate 4). The server never installs a session
    // on a device that does not advertise this, so a device that has not opted
    // in is simply never asked — and a build without the overlay features has
    // no relay to advertise at all.
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    if tunnel_core::overlay::orgrelay::relay_server_enabled() {
        caps.push(RpcCap::RelayServer);
    }
    // FR-40 — a build with an overlay surface can retire its overlay key on
    // order. A property of the BUILD, like `config`: the device may still
    // refuse (`overlay_key_rotation=false`), and says so in its report.
    #[cfg(any(feature = "overlay-l3", feature = "overlay-netstack"))]
    caps.push(RpcCap::KeyRotate);
    // FR-90 — a build that runs agent sessions answers `rc:hive.start` and
    // reports with `rc:hive.state`. A property of the BUILD, like `ssh`: the
    // device still refuses unless its owner switched `hive_enabled` on, and
    // says so in the ack. Every `hive_host` build: Linux, macOS, Windows.
    #[cfg(hive_host)]
    caps.push(RpcCap::Hive);
    // FR-90 P0d-2 — and serves a session's transcript to a browser over a
    // viewer peer: answers `rc:hive.view.grant` before the browser dials.
    #[cfg(hive_host)]
    caps.push(RpcCap::HiveView);
    // FR-90 P1e — and understands a session's core memory (`rc:hive.memory`).
    // A property of the BUILD: whether a session ever sees it is the device's
    // own `hive_core_memory`, default off.
    #[cfg(hive_host)]
    caps.push(RpcCap::HiveMemory);
    // FR-90 P1j — and mirrors terminal sessions its people adopted. NOT a
    // property of the build alone: advertised only while the device's owner
    // allows it (`hive_adopt`), and the server honours an offer only from a
    // connection that advertises it.
    #[cfg(hive_host)]
    if crate::hive::adopt_enabled() {
        caps.push(RpcCap::HiveAdopt);
    }
    // FR-90 P2c-3 — and holds copies of its owner's sessions run elsewhere:
    // answers `rc:hive.replica.join`. Not a property of the build alone:
    // only while the owner's `hive_replica` is on and the store is open, and
    // on the primary enrollment's connection only (the hello strips both
    // words from any other). `hive-archive` only ever with it.
    #[cfg(hive_host)]
    if crate::hive::replica_enabled() {
        caps.push(RpcCap::HiveReplica);
    }
    #[cfg(hive_host)]
    if crate::hive::archive_enabled() {
        caps.push(RpcCap::HiveArchive);
    }
    // FR-90 P2i-1 — and answers a search over those copies, as an archive
    // replica: `hive-search`, only with `hive-archive` and `hive-replica`,
    // which the device's own gate asks again at each grant.
    #[cfg(hive_host)]
    if crate::hive::archive_enabled() && crate::hive::replica_enabled() {
        caps.push(RpcCap::HiveSearch);
    }
    // FR-90 P2d-3b — and carries the replica stream: serves a member's grant
    // as a source and follows a dial as a member. While its store is open,
    // whatever its owner says of holding copies: a primary serves its own
    // sessions' members. Stripped from any connection but the primary
    // enrollment's (the hello), like every Hive word.
    #[cfg(hive_host)]
    if crate::hive::stream_enabled() {
        caps.push(RpcCap::HiveStream);
    }
    // FR-90 P2e-2 — and holds a leased session to the server's word on each
    // connection (`rc:hive.lease`): after a restart or a reconnect it runs
    // one only once the server has said so there. A property of the BUILD,
    // like `hive`, and kept on a secondary org's connection like it: that
    // org's Hive frames are refused there either way.
    #[cfg(hive_host)]
    caps.push(RpcCap::HiveLease);
    // FR-90 P2e-5c — and takes part in a promotion at either end: as the
    // primary it drains (`rc:hive.drain`), as the member it prepares
    // (`rc:hive.prepare`) and takes the session over from its copy at the
    // move (`rc:hive.move`), and its joins take where each superseded fence
    // ended (P2e-5b). A property of the BUILD, like `hive-lease`: the device
    // still refuses a drain or a prepare its own gates do not pass, in its
    // own words, and a secondary org's connection refuses both.
    #[cfg(hive_host)]
    caps.push(RpcCap::HivePromote);
    // FR-90 P2f-1a — and takes part in a teleport, a move to a device that
    // holds no copy yet: as the target, a teleport's prepare passes every
    // start gate but the copy and admits that one session's join, whatever
    // `hive_replica` says; as the primary, a join in the primary's role
    // checkpoints at once when no turn runs and says where its copy ends. A
    // property of the BUILD, like `hive-promote`, and kept on a secondary
    // org's connection like it: no placement chooses a teleport's target, and
    // that org's prepare and join are refused there either way.
    #[cfg(hive_host)]
    caps.push(RpcCap::HiveTeleport);
    // FR-90 P2f-3a — and starts a fork of a session it holds a copy of: a
    // start carrying `fork_of` takes the parent's last checkpoint under the
    // fork's ids and launches `--resume <parent> --fork-session --session-id
    // <fork>`. A property of the BUILD; a build before it would start the
    // fork empty.
    #[cfg(hive_host)]
    caps.push(RpcCap::HiveFork);
    // FR-90 P2f-4a — and makes a terminal session it adopted, whose terminal
    // ended here, a managed one: a start carrying `from_terminal` puts the
    // terminal's own history in place as the account it ran as, by this
    // device's record of whose terminal it was, and resumes it. A property of
    // the BUILD, Unix only: Windows adopts since FR-97 P4c, but putting a
    // terminal's history in place runs as the account through Unix tools. The
    // device's start gates still decide, `hive_enabled` first.
    #[cfg(all(hive_host, unix))]
    caps.push(RpcCap::HiveManage);
    // FR-90 P2g-1 — and purges a session on `rc:hive.purge`, answering per
    // part: its own copy, then as the session's account its state directory
    // and its checkpoints' refs. A property of the BUILD, on every platform
    // that runs sessions.
    #[cfg(hive_host)]
    caps.push(RpcCap::HivePurge);
    // FR-90 P2g-4a — and purges every session it holds on
    // `rc:hive.purge_all`, which its enrollment's removal sends. A property
    // of the BUILD, as `hive-purge` is.
    #[cfg(hive_host)]
    caps.push(RpcCap::HivePurgeAll);
    // #2294 — and finishes that purge before it exits on the removal's
    // goodbye (`hive::finish_removal_purge`), so the server's kick may tell
    // it. A property of the BUILD, as `hive-purge-all` is.
    #[cfg(hive_host)]
    caps.push(RpcCap::HivePurgeDrain);
    // FR-90 P4-3b-1 — and scans the roots its owner lists on
    // `rc:knowhow.scan`, each as the account that owns it. A property of the
    // BUILD, as `hive-purge` is: an empty `knowhow_scan_roots` is an answer
    // (`no_roots`), never a reason to stay silent. Linux and macOS; Windows
    // advertises it from P4-3b-4.
    #[cfg(all(hive_host, unix))]
    caps.push(RpcCap::KnowhowScan);
    // FR-90 P4-4-1b — and its sessions can ask the organization's map, so
    // its server tells it at each hello whether the map is there for them
    // (`rc:knowhow.served`, decision 49). A property of the BUILD: every OS
    // that runs sessions, the toolbelt's tools apart from the scanner.
    #[cfg(hive_host)]
    caps.push(RpcCap::KnowhowTools);
    // FR-90 P4-5-1b — and probes, on `rc:knowhow.probe`, what the map says
    // within what its owner lists in `knowhow_probe_reach`. A property of the
    // BUILD, as `knowhow-scan` is: an empty list is an answer
    // (`not_allowed`), never a reason to stay silent. Every OS that runs
    // sessions.
    #[cfg(hive_host)]
    caps.push(RpcCap::KnowhowProbe);
    // ⚠️ FR-99's `privacy-*` words are NOT here: this list is memoized at boot
    // (`detect()`), and those follow the owner's live gate and the overlay
    // coming up. `signaling::stub_caps` adds them at every announcement.
    caps.into_iter().map(|c| c.wire().to_string()).collect()
}

/// Multi-user P3 — how many CONCURRENT remote-control sessions this agent
/// advertises (the server's `AgentBusy` gate enforces the value). Pinned to
/// 1 since v1; the whole capacity model (server gate, per-session
/// `AgentPeer`s, per-session consent/permissions) was already N-ready.
///
/// Default 2 — pair-working: each extra session runs its OWN capture +
/// encoder until the P5 shared-floor encoder lands, so weak-GPU hosts
/// (Iris-Xe-class) may prefer `rc_max_sessions = 1`; the DXGI
/// duplication app-limit (~4) bounds the useful ceiling — clamp at 8.
/// Concurrent-INPUT chaos is prevented server-side (one INPUT holder per
/// agent until the P6 arbitration modes land).
///
/// `ROOMLERD_RC_MAX_SESSIONS` env > `rc_max_sessions` config key
/// (bridged via the S2 fallback map) > built-in 2.
pub(crate) fn rc_max_sessions() -> u8 {
    tunnel_core::env::node_env("RC_MAX_SESSIONS")
        .and_then(|v| v.trim().parse::<u8>().ok())
        .map(|n| n.clamp(1, 8))
        .unwrap_or(2)
}

/// Codec to probe. We only probe codecs that fail closed on activation
/// error (HEVC + AV1 today); H.264 has a working triple-fallback path
/// and is not gated.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
#[derive(Debug, Clone, Copy)]
enum CodecProbe {
    /// FR-77 — probed for the CELL only (`video_cells` must say whether the
    /// cascade lands on silicon); the legacy `mf-h264-hw` label stays
    /// enumeration-gated as it always was.
    H264,
    Hevc,
    Av1,
}

#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
impl CodecProbe {
    /// The name the denylist knows this cell by.
    fn cell(self) -> &'static str {
        match self {
            Self::H264 => super::cells::MF_H264,
            Self::Hevc => super::cells::MF_HEVC,
            Self::Av1 => super::cells::MF_AV1,
        }
    }
}

/// Outcome of a codec probe. We split SW from HW because shipping
/// HEVC over the SW MFT (`HEVCVideoExtensionEncoder`) is a UX
/// regression vs negotiating H.264 with the host's HW H.264 path
/// (Intel QuickSync, NVENC, AMF). Two reasons: chroma artefacts at
/// low bitrate, and roughly 3x CPU cost vs HW H.264. Field reports
/// 2026-04-24 and 2026-04-26 from boxes where the IHV HEVC MFT
/// (Intel Hardware H265 Encoder MFT) fails ActivateObject 0x80004005
/// and the cascade falls to SW HEVC. Demoting those hosts out of
/// HEVC advertising forces the browser to negotiate H.264 where the
/// cascade lands on real HW.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeResult {
    /// Cascade landed on dedicated HW MFT — codec is safe to advertise.
    Hardware,
    /// Cascade activated, but only on the SW fallback (`backend="sw"`).
    /// Caller decides whether to advertise; default policy is to drop
    /// HEVC/AV1 when SW-only and let H.264 win negotiation.
    SoftwareOnly,
    /// No working encoder found at all. Caller MUST drop from caps.
    Failed,
}

/// How long [`activates`] waits for an MF encoder's worker to finish its
/// teardown. It runs on the worker's thread, so without a wait a fault there
/// lands in whatever open comes next and is blamed on that cell (#3267). A
/// teardown that outlasts this is left to run, as every drop leaves it.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
const MF_TEARDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Spin up the real MF encoder for `codec` at a tiny probe resolution,
/// inspect the resulting backend kind, then drop it. Logs the verdict
/// at info / warn so the cascade outcome is visible in startup logs.
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
fn activates(codec: CodecProbe) -> ProbeResult {
    let _window = child::OpenWindow::begin(codec.cell(), ChromaFormat::Yuv420);
    let start = std::time::Instant::now();
    let result = match codec {
        CodecProbe::H264 => super::mf::MfEncoder::new_h264(PROBE_WIDTH, PROBE_HEIGHT),
        CodecProbe::Hevc => super::mf::MfEncoder::new_hevc(PROBE_WIDTH, PROBE_HEIGHT),
        CodecProbe::Av1 => super::mf::MfEncoder::new_av1(PROBE_WIDTH, PROBE_HEIGHT),
    };
    let elapsed_ms = start.elapsed().as_millis();
    match result {
        Ok(enc) => {
            use super::VideoEncoder;
            let is_hw = enc.is_hardware();
            // The worker's Shutdown cmd runs MFShutdown + CoUninitialize on
            // its thread; this waits for it, inside the announced window.
            enc.shut_down_within(MF_TEARDOWN_WAIT);
            if is_hw {
                tracing::info!(
                    codec = ?codec,
                    elapsed_ms,
                    "caps probe: codec activates on HW — advertising"
                );
                ProbeResult::Hardware
            } else {
                tracing::warn!(
                    codec = ?codec,
                    elapsed_ms,
                    "caps probe: codec activates only on SW — NOT advertising (H.264 HW likely better). Set ROOMLERD_ALLOW_SW_HEAVY=1 to override."
                );
                ProbeResult::SoftwareOnly
            }
        }
        Err(e) => {
            tracing::warn!(
                codec = ?codec,
                %e,
                elapsed_ms,
                "caps probe: codec enumerates but does NOT activate — NOT advertising"
            );
            ProbeResult::Failed
        }
    }
}

/// Operator escape hatch: advertise HEVC/AV1 even when the cascade
/// only lands on SW. Off by default. Useful when the host has no
/// working H.264 HW path and SW HEVC is a strict improvement over
/// SW H.264 (rare but possible on machines without Intel QSV / NVENC
/// / AMF).
#[cfg(all(target_os = "windows", feature = "mf-encoder"))]
fn allow_sw_heavy_override() -> bool {
    use tunnel_core::env::node_env;
    node_env("ALLOW_SW_HEAVY")
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// Intersection + priority for codec negotiation (Phase 2 2B.2).
/// Takes the browser-advertised codec list + the agent's supported
/// codec list, returns the best codec both sides support.
///
/// Priority order: **av1 > h265 > vp9 > h264 > vp8**. AV1 + HEVC
/// cut 30-50% off the bitrate at equal quality vs H.264; VP9 is
/// closer to H.264 but natively supported in every WebRTC stack so
/// we prefer it over H.264 when available. H.264 is the universal
/// fallback.
///
/// Returns `"h264"` on empty inputs — maintains back-compat with
/// pre-2B.1 browsers that don't advertise anything.
pub fn pick_best_codec(browser_caps: &[String], agent_caps: &[String]) -> String {
    const PRIORITY: &[&str] = &["av1", "h265", "vp9", "h264", "vp8"];
    let browser_has = |c: &str| browser_caps.iter().any(|b| b.eq_ignore_ascii_case(c));
    let agent_has = |c: &str| agent_caps.iter().any(|a| a.eq_ignore_ascii_case(c));
    for candidate in PRIORITY {
        if browser_has(candidate) && agent_has(candidate) {
            return (*candidate).to_string();
        }
    }
    // Fallback — universal baseline. If the browser advertises nothing
    // (pre-2B.1 controller) we assume it decodes H.264.
    "h264".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FR-119 P1 — `fps-high` is off unless `ROOMLERD_FPS_HIGH=1`: until P2
    /// builds the grant, a viewer must not be offered a picker the agent can
    /// never honour. `chunk-framing` stays unconditional (FR-17).
    #[test]
    fn fps_high_is_not_advertised_by_default() {
        if tunnel_core::env::node_env("FPS_HIGH").is_some() {
            return; // a host that set it asked for it; nothing to check here
        }
        let features = video_features();
        assert!(features.iter().any(|f| f == "chunk-framing"));
        assert!(
            !crate::encode::frame_rate::any_verb_in(&features),
            "a frame-rate verb advertised with the switch unset: {features:?}"
        );
    }

    /// FR-130 P1 — `color-described` rides the hello exactly while the
    /// encoders write the description: by default, and not with
    /// `color_description` off.
    #[test]
    fn color_described_follows_the_switch() {
        use tunnel_core::env::test_env::{Saved, set};
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _switch = Saved::cleared("COLOR_DESCRIPTION");
        let has = || video_features().iter().any(|f| f == COLOR_DESCRIBED);
        assert!(has(), "on by default");
        // SAFETY: serialised by DENY_ENV_LOCK; the guard restores it.
        unsafe { set("COLOR_DESCRIPTION", "0") };
        assert!(!has(), "off with the switch off");
        assert!(video_features().iter().any(|f| f == "chunk-framing"));
    }

    /// FR-84 D4 — the LocalAPI's read of the encoder matrix must never be
    /// what launches the probe. `summarize_cached` reads the `OnceLock` and
    /// reports `not_probed` while it is empty; a version that reached for
    /// `detect()` to have something to show would spawn the probe children
    /// from a control-surface poll on every host that has not connected yet
    /// — vendor driver code, run because someone opened the Overview.
    ///
    /// The lock is read DIRECTLY, not through `cached()`: a `cached()` that
    /// called `detect()` would fill the lock before `before` was taken and
    /// pass. (Negative control, run for the PR: `cached()` returning
    /// `Some(detect())` turns this red.) No other lib test fills the lock.
    #[test]
    fn not_probed_never_triggers_a_probe() {
        // It reads `ENCODER_CELLS_DENY` twice (inside `summarize_cached` and
        // in the final `assert_eq!`), and three tests rewrite that var under
        // DENY_ENV_LOCK; without the lock, one of their writes can land
        // between the two reads.
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let before = CACHED_CAPS.get().is_some();
        let summary = summarize_cached();
        let after = CACHED_CAPS.get().is_some();
        assert_eq!(
            before, after,
            "reading the summary must not populate the probe cache"
        );
        match (ENCODERS_COMPILED, before) {
            (false, _) => assert_eq!(summary.state, "unsupported"),
            (true, false) => {
                assert_eq!(summary.state, "not_probed");
                assert!(summary.cells.is_empty());
                assert!(summary.hw_encoders.is_empty());
                assert_eq!(summary.probe_ms, None);
                assert!(!summary.probe_cached);
            }
            (true, true) => assert_eq!(summary.state, "ready"),
        }
        // The denylist and the preference are facts about the process, not
        // about the probe: reported whatever the state.
        assert_eq!(summary.denied, denied_cells());
        assert_eq!(
            summary.encoder_preference.as_deref(),
            super::super::resolved_preference().map(|p| p.wire())
        );
    }

    /// FR-84 D4 — a populated caps value maps cell for cell onto the wire
    /// summary: codec / backend / chroma / hardware, the legacy labels, the
    /// probe timing, plus the denylist and preference the caller supplies.
    #[test]
    fn summary_of_a_populated_caps_carries_every_cell() {
        let caps = AgentCaps {
            hw_encoders: vec!["ffmpeg-hevc_nvenc".into(), "openh264-sw".into()],
            codecs: vec!["h264".into(), "h265".into()],
            video_cells: vec![
                VideoCell::new(
                    VideoCodec::Hevc,
                    VideoBackend::Nvenc,
                    &[ChromaFormat::Yuv420, ChromaFormat::Yuv444],
                    true,
                ),
                VideoCell::new(
                    VideoCodec::H264,
                    VideoBackend::Openh264,
                    &[ChromaFormat::Yuv420],
                    false,
                ),
            ],
            probe_ms: Some(812),
            probe_cached: true,
            ..Default::default()
        };
        let s = summarize(
            Some(&caps),
            vec!["hevc_qsv:yuv444".into()],
            Some("hardware"),
        );
        assert_eq!(
            s.state,
            if ENCODERS_COMPILED {
                "ready"
            } else {
                "unsupported"
            }
        );
        assert_eq!(s.cells.len(), 2);
        assert_eq!(s.cells[0].codec, "hevc");
        assert_eq!(s.cells[0].backend, "nvenc");
        assert_eq!(s.cells[0].chroma, vec!["yuv420", "yuv444"]);
        assert!(s.cells[0].hardware);
        assert_eq!(s.cells[1].backend, "openh264");
        assert!(!s.cells[1].hardware);
        assert_eq!(s.hw_encoders, caps.hw_encoders);
        assert_eq!(s.codecs, caps.codecs);
        assert_eq!(s.probe_ms, Some(812));
        assert!(s.probe_cached);
        assert_eq!(s.denied, vec!["hevc_qsv:yuv444"]);
        assert_eq!(s.encoder_preference.as_deref(), Some("hardware"));
        // No caps at all: the process facts still travel, nothing invented.
        let none = summarize(None, Vec::new(), None);
        assert!(none.cells.is_empty() && none.probe_ms.is_none());
        assert_eq!(none.encoder_preference, None);
    }

    /// Closes the loop the `RpcCap` enum exists to close: every verb this
    /// agent PUTS ON THE WIRE must be one the server can parse back. A typo
    /// here used to be invisible — the device would simply look like it lacked
    /// the feature, which is indistinguishable from an old agent.
    #[test]
    fn every_advertised_verb_is_a_known_capability() {
        use roomler_ai_remote_control::models::RpcCap;

        let advertised = rpc_caps();
        assert!(
            !advertised.is_empty(),
            "every build advertises exec at least"
        );
        for verb in &advertised {
            assert!(
                RpcCap::from_wire(verb).is_some(),
                "advertised {verb:?} is not a verb the server knows"
            );
        }
        // Build-independent floor: the exec engine is plain process spawning,
        // so these hold in every feature configuration.
        assert!(advertised.iter().any(|v| v == RpcCap::Exec.wire()));
        assert!(advertised.iter().any(|v| v == RpcCap::Originate.wire()));

        // The SSH pair is feature-gated TOGETHER: advertising `ssh` without
        // `ssh-consent` is precisely the rc.419 state the server has to refuse
        // a consent policy for, so they must never drift apart in one build.
        assert_eq!(
            advertised.iter().any(|v| v == RpcCap::Ssh.wire()),
            advertised.iter().any(|v| v == RpcCap::SshConsent.wire()),
            "ssh and ssh-consent must be advertised together in a single build"
        );
        assert_eq!(
            advertised.iter().any(|v| v == RpcCap::Ssh.wire()),
            cfg!(feature = "ssh-server"),
            "the ssh verbs must track the ssh-server feature, not the version"
        );
        // FR-83 — and the ack travels with them. A build advertising
        // `ssh-grant-ack` without an SSH server would have the server wait on
        // grants nothing records; one serving SSH without it would put every
        // caller back in the #1597 race.
        assert_eq!(
            advertised.iter().any(|v| v == RpcCap::SshGrantAck.wire()),
            cfg!(feature = "ssh-server"),
            "ssh-grant-ack must be advertised exactly when the SSH server is built"
        );
        // FR-90 P3-8-2a — and so does one command: the server sends an agent
        // session's grant only where this is said, and the SSH server is what
        // holds it there.
        assert_eq!(
            advertised.iter().any(|v| v == RpcCap::SshOneCommand.wire()),
            cfg!(feature = "ssh-server"),
            "ssh-one-command must be advertised exactly when the SSH server is built"
        );
        // FR-97 P5a — `console-user` on every Unix build and never on
        // Windows, whose `console_user` is its SYSTEM service's: a Windows
        // worker sent `run_as` would answer for an identity it never offered.
        assert_eq!(
            advertised.iter().any(|v| v == RpcCap::ConsoleUser.wire()),
            cfg!(unix),
            "console-user must be advertised exactly on Unix builds"
        );

        // FR-52 — build-independent, like `config`, and NOT derived from
        // `external_access_enabled`. The server gates the admin approval on
        // this verb, so an agent that stopped sending it would make every
        // device in the fleet un-approvable for cross-org access — a silent
        // fleet-wide close, not an error anyone would see.
        assert!(
            advertised
                .iter()
                .any(|v| v == RpcCap::ExternalAccess.wire()),
            "external-access must be advertised by every build: it says this \
             agent can tell the person at the machine that a controller is \
             from outside their organization, which is what makes gate 2 \
             approvable at all"
        );

        // FR-52 P4 — but `external-session` exactly when the build can bind a
        // session to a login. Advertised without it, the server would open
        // external sessions a device then refuses every time; withheld from a
        // build that has it, the feature is dark on that device.
        assert_eq!(
            advertised
                .iter()
                .any(|v| v == RpcCap::ExternalSession.wire()),
            cfg!(feature = "external-access"),
            "external-session must be advertised exactly when the external-access \
             stack is built"
        );

        // FR-100 P1b — `peer-auth` in every build: the signing needs no
        // feature, only a device key, and the hello says whether there is one.
        assert!(
            advertised.iter().any(|v| v == RpcCap::PeerAuth.wire()),
            "peer-auth must be advertised by every build"
        );
    }

    /// Permissions are always REPORTED, even when everything is granted.
    ///
    /// `None` means "this agent is too old to know", and a server or UI must
    /// be able to tell that apart from "this agent holds nothing" — so a
    /// current agent must never send `None`. The list itself is
    /// platform-dependent (only macOS actually gates these), which is why this
    /// asserts the reporting contract rather than the contents.
    #[test]
    fn caps_always_report_permission_state() {
        let caps = compute_caps(false, false);
        let perms = caps
            .permissions
            .expect("a current agent must report permissions, so None can keep meaning 'unknown'");
        for p in &perms {
            assert!(
                matches!(p.as_str(), "screen-capture" | "input"),
                "unrecognised permission {p:?} — readers skip unknown values, so a typo \
                 here silently reads as 'not granted'"
            );
        }
        // Platforms with no permission model must not look muzzled.
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            perms,
            vec!["screen-capture".to_string(), "input".to_string()],
            "a platform that does not gate these must report both granted"
        );
    }

    /// `has_input_permission` must track the OS, not the build.
    ///
    /// It was `cfg!(feature = "enigo-input")` — a compile-time constant — so a
    /// Mac with Accessibility denied advertised working input while silently
    /// dropping every event. Without the feature it must stay false regardless.
    #[test]
    fn input_permission_is_not_merely_a_compile_flag() {
        let caps = compute_caps(false, false);
        if !cfg!(feature = "enigo-input") {
            assert!(!caps.has_input_permission);
        }
        assert_eq!(
            caps.has_input_permission,
            cfg!(feature = "enigo-input") && input_permission_granted(),
            "the advertised bool must agree with the runtime probe"
        );
    }

    /// THE property this slice exists for: a host whose hardware probes
    /// cannot be trusted still produces usable caps, and never claims a codec
    /// it has no evidence for.
    ///
    /// `compute_caps(false, false)` is exactly what `detect()` falls back to when the
    /// probe child dies, so this is the shape a driver fault now yields —
    /// where before it yielded a crash-looping daemon.
    #[test]
    fn a_failed_probe_still_yields_usable_caps_and_advertises_no_hardware() {
        let caps = compute_caps(false, false);

        // Nothing that required asking a driver.
        for name in ["mf-h264-hw", "mf-h265-hw", "mf-av1-hw"] {
            assert!(
                !caps.hw_encoders.iter().any(|h| h == name),
                "{name} was advertised without a successful probe: {:?}",
                caps.hw_encoders
            );
        }
        assert!(
            !caps.hw_encoders.iter().any(|h| h.starts_with("ffmpeg-")),
            "an ffmpeg HW encoder was advertised without a successful probe: {:?}",
            caps.hw_encoders
        );
        // Transports are the sharp end — negotiating one the host cannot
        // fulfil is a black session, not a downgrade.
        for t in [
            "data-channel-hevc",
            "data-channel-av1",
            "data-channel-h264",
            "data-channel-vp9-444",
        ] {
            assert!(
                !caps.transports.iter().any(|x| x == t),
                "{t} was advertised without a successful probe: {:?}",
                caps.transports
            );
        }

        // ...but the agent is still a working agent: the parts that never
        // depended on a driver survive, so a probe fault degrades the host
        // rather than disabling it. File transfer is the clearest case, and
        // since #1672 it is not even IN the probe's answer: `detect()` assigns
        // `files` in the daemon after the probe has answered or failed, so no
        // probe outcome can reach it. The "upload survives" assertion that
        // used to sit here moved to `the_daemon_assigns_every_config_list`,
        // which runs this same fallback struct through that assignment.
        assert!(caps.max_simultaneous_sessions > 0);

        // FR-77 — no hardware cell without a probe, and no probe time either
        // (the parent stamps it only when the child came back).
        assert!(
            caps.video_cells.iter().all(|c| !c.hw),
            "a hardware cell was advertised without a probe: {:?}",
            caps.video_cells
        );
        assert!(caps.probe_ms.is_none());
        // Whatever software cells survive must speak the vocabulary.
        for cell in &caps.video_cells {
            assert!(cell.typed().is_some(), "{cell:?} is outside the vocabulary");
        }
    }

    #[test]
    fn picks_av1_when_both_sides_support() {
        let chosen = pick_best_codec(
            &["h264".into(), "av1".into(), "h265".into()],
            &["h264".into(), "av1".into(), "h265".into()],
        );
        assert_eq!(chosen, "av1");
    }

    #[test]
    fn picks_h265_over_h264_when_browser_lacks_av1() {
        let chosen = pick_best_codec(
            &["h264".into(), "h265".into()],
            &["h264".into(), "av1".into(), "h265".into()],
        );
        assert_eq!(chosen, "h265");
    }

    #[test]
    fn picks_h264_when_only_common_codec() {
        let chosen = pick_best_codec(&["h264".into()], &["h264".into(), "h265".into()]);
        assert_eq!(chosen, "h264");
    }

    #[test]
    fn falls_back_to_h264_on_empty_browser_caps() {
        // Pre-2B.1 controller that doesn't advertise anything.
        let chosen = pick_best_codec(&[], &["h264".into(), "h265".into()]);
        assert_eq!(chosen, "h264");
    }

    #[test]
    fn falls_back_to_h264_on_no_intersection() {
        // Browser advertises only VP8, agent only H.264. No overlap;
        // we return h264 so the caller has a usable default.
        let chosen = pick_best_codec(&["vp8".into()], &["h264".into()]);
        assert_eq!(chosen, "h264");
    }

    #[test]
    fn case_insensitive_match() {
        let chosen = pick_best_codec(&["H264".into(), "H265".into()], &["h265".into()]);
        assert_eq!(chosen, "h265");
    }

    #[test]
    fn prefers_vp9_over_h264() {
        let chosen = pick_best_codec(
            &["h264".into(), "vp9".into()],
            &["h264".into(), "vp9".into()],
        );
        assert_eq!(chosen, "vp9");
    }

    /// Y.4: in default-feature (no `vp9-444`) builds, the transports
    /// list must NOT advertise `data-channel-vp9-444`. The browser
    /// reads this list to decide whether to even open the DC; an
    /// agent that lies about transport support would crash the
    /// session at media-pump time.
    #[cfg(not(feature = "vp9-444"))]
    #[test]
    fn detect_omits_vp9_444_transport_when_feature_disabled() {
        // Probing ENABLED on purpose: the claim is that a build without the
        // feature never advertises the transport even when probes run. (The
        // vp9 probe itself is cfg'd out here, so nothing is actually opened.)
        let caps = compute_caps(true, true);
        assert!(
            !caps.transports.iter().any(|t| t == "data-channel-vp9-444"),
            "default-feature build advertised vp9-444 transport: {:?}",
            caps.transports
        );
        assert!(
            !caps.hw_encoders.iter().any(|e| e == "libvpx-vp9-444-sw"),
            "default-feature build advertised libvpx encoder: {:?}",
            caps.hw_encoders
        );
    }

    /// Y.4 caps probe (post Y.runtime-encoder rewrite, 0.1.47): the
    /// libvpx encoder at the probe resolution must successfully
    /// activate, and `compute_caps` must surface both the
    /// `data-channel-vp9-444` transport and the `libvpx-vp9-444-sw`
    /// encoder label. CI runs this with `libvpx-dev` apt-installed
    /// so the link succeeds. If the probe ever regresses (libvpx
    /// missing on the build host, encoder init failure on the probe
    /// dimensions), this test catches it before a session ever
    /// negotiates onto a broken transport.
    #[cfg(feature = "vp9-444")]
    #[test]
    fn detect_advertises_vp9_444_transport_when_encoder_works() {
        // Probing ENABLED: this test's whole claim is that the probe runs and
        // succeeds. libvpx is our own software encoder, not a vendor driver,
        // so running it in-process here is safe.
        let caps = compute_caps(true, true);
        assert!(
            caps.transports.iter().any(|t| t == "data-channel-vp9-444"),
            "vp9-444 transport must be advertised when libvpx probe succeeds; got {:?}",
            caps.transports
        );
        assert!(
            caps.hw_encoders.iter().any(|e| e == "libvpx-vp9-444-sw"),
            "libvpx encoder label must be advertised when probe succeeds; got {:?}",
            caps.hw_encoders
        );
    }

    /// Audio caps: a build WITH the `audio` feature must advertise
    /// `opus` so the browser knows it may request `audio_enabled`.
    #[cfg(feature = "audio")]
    #[test]
    fn detect_advertises_opus_audio_when_feature_enabled() {
        // Feature-gated, never probe-gated — no driver call needed.
        let caps = compute_caps(false, false);
        assert!(
            caps.audio.iter().any(|c| c == "opus"),
            "audio-feature build must advertise opus; got {:?}",
            caps.audio
        );
    }

    /// Audio caps: a default (no `audio` feature) build must NOT
    /// advertise any audio codec — the agent adds no audio track, so a
    /// browser that saw `opus` and asked for `audio_enabled` would wait
    /// forever for a track that never arrives.
    #[cfg(not(feature = "audio"))]
    #[test]
    fn detect_omits_audio_when_feature_disabled() {
        // Audio advertisement is feature-gated, never probe-gated, so this
        // needs no driver call.
        let caps = compute_caps(false, false);
        assert!(
            caps.audio.is_empty(),
            "default build must not advertise audio; got {:?}",
            caps.audio
        );
    }

    /// rc.19 file-DC v3 capability lock. The browser opts into
    /// resumable uploads ONLY when this string appears in
    /// `caps.files`. Removing or renaming it would silently disable
    /// the resume path for every rc.19+ browser — lock here.
    #[test]
    fn detect_advertises_resume_files_cap() {
        // File caps never depended on a probe, and since #1672 they are the
        // daemon's list (`files_caps`), whatever the browse flag says.
        for browse in [false, true] {
            let files = files_caps_for(browse);
            assert!(
                files.iter().any(|s| s == "resume"),
                "rc.19 caps.files must include \"resume\" (browse={browse}); got {files:?}"
            );
        }
    }

    /// #1672 — `files` is the DAEMON's to assign, in `detect()`, never the
    /// caps-probe child's to compute. On a cache miss `cached_or_probed`
    /// returns the child's struct wholesale, and the child loads no config:
    /// with the files block inside `compute_caps`, `browse` rode the flag's
    /// built-in default (`true`) and a device with `enable_remote_browse =
    /// false` advertised it on every release's first boot and on every boot
    /// of a host with no hardware cell. So what the child runs must leave the
    /// field empty — a re-added block here is exactly the regression, and
    /// this is red on it.
    #[test]
    fn files_are_assigned_by_the_daemon_never_by_the_probe_child() {
        let caps = compute_caps(false, false);
        assert!(
            caps.files.is_empty(),
            "compute_caps is what the caps-probe child runs; it must leave `files` to \
             detect(), which reads the browse flag in the daemon — got {:?}",
            caps.files
        );
    }

    /// #1672 — the daemon's list follows the CONFIG: `browse` present exactly
    /// when remote browse is enabled, and the four build-constant verbs in
    /// both cases (an empty or shortened list would send browsers back to
    /// the upload-only fallback). Driven through `files_caps_for` so the
    /// process-global flag is never flipped under the other tests.
    #[test]
    fn files_caps_follow_the_config() {
        let off = files_caps_for(false);
        assert_eq!(
            off,
            vec!["upload", "download", "download-folder", "resume"],
            "browse disabled: the four always-built verbs and nothing else"
        );
        let on = files_caps_for(true);
        assert_eq!(
            on,
            vec!["upload", "download", "download-folder", "resume", "browse"],
            "browse enabled: the same four, plus `browse`"
        );
    }

    // ─── FR-97 P0c — a word is a promise this host keeps ─────────────────
    // Each test below fails on master, where every one of these words was a
    // compile-time constant (the census before P0c, FR-97 §8).

    fn p0c_facts(os: roomler_ai_remote_control::models::OsKind) -> HostFacts {
        HostFacts {
            os,
            audio_built: true,
            audio_backend: true,
            linux_audio_refusal: None,
            mac_audio_refusal: None,
            watches_clipboard_images: true,
            clipboard_reachable: true,
            layout_reachable: true,
            layout_switchable: true,
            display_switchable: true,
        }
    }

    fn p0c_caps() -> AgentCaps {
        AgentCaps {
            audio: vec!["opus".into()],
            supports_clipboard: true,
            clipboard: ["ack", "events", "images", "html"]
                .map(String::from)
                .to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn a_host_without_an_audio_backend_offers_no_track_and_says_why() {
        use roomler_ai_remote_control::models::OsKind;
        // A Windows build without its backend says so plainly.
        let windows = with_honest_words(
            p0c_caps(),
            &HostFacts {
                audio_backend: false,
                ..p0c_facts(OsKind::Windows)
            },
        );
        assert!(
            windows.audio.is_empty(),
            "no backend, no opus (R1): {:?}",
            windows.audio
        );
        assert_eq!(windows.unavailable["audio"].code, "no-backend");
        // FR-97 P2a — a Mac with ScreenCaptureKit offers the track; one that
        // cannot capture says which of its two reasons holds.
        let mac = with_honest_words(p0c_caps(), &p0c_facts(OsKind::Macos));
        assert_eq!(mac.audio, vec!["opus".to_string()]);
        assert!(!mac.unavailable.contains_key("audio"));
        use crate::audio_source::AudioUnavailableCode as MacWhy;
        for (why, code, names) in [
            (MacWhy::NoSession, "no-session", "GUI worker"),
            (MacWhy::SwitchedOff, "switched-off", "ROOMLERD_SCK_AUDIO=0"),
        ] {
            let c = with_honest_words(
                p0c_caps(),
                &HostFacts {
                    audio_backend: false,
                    mac_audio_refusal: Some(why),
                    ..p0c_facts(OsKind::Macos)
                },
            );
            assert!(c.audio.is_empty(), "{why:?}: {:?}", c.audio);
            assert_eq!(c.unavailable["audio"].code, code, "{why:?}");
            assert!(c.unavailable["audio"].reason.contains(names), "{why:?}");
        }
        // A Linux host with a way to the computer's sound.
        let linux = with_honest_words(p0c_caps(), &p0c_facts(OsKind::Linux));
        assert_eq!(linux.audio, vec!["opus".to_string()]);
        assert!(!linux.unavailable.contains_key("audio"));
        // FR-97 P3c — and without one: no track, rather than the microphone
        // every Linux session carried before (measured 2026-10-17), and the
        // route's own reason.
        use crate::audio_source::AudioUnavailableCode as Why;
        for (why, code, names) in [
            (Why::NoMonitor, "no-monitor", "ROOMLERD_AUDIO_SESSION=0"),
            (Why::NoSoundServer, "no-sound-server", "libpulse-simple"),
            (Why::NoSession, "no-session", "session helper"),
        ] {
            let c = with_honest_words(
                p0c_caps(),
                &HostFacts {
                    audio_backend: false,
                    linux_audio_refusal: Some(why),
                    ..p0c_facts(OsKind::Linux)
                },
            );
            assert!(c.audio.is_empty(), "{why:?}: {:?}", c.audio);
            assert_eq!(c.unavailable["audio"].code, code, "{why:?}");
            assert!(c.unavailable["audio"].reason.contains(names), "{why:?}");
        }
        // A build without the feature had nothing to claim, and says nothing.
        let none = with_honest_words(
            AgentCaps {
                audio: Vec::new(),
                ..p0c_caps()
            },
            &HostFacts {
                audio_built: false,
                audio_backend: false,
                ..p0c_facts(OsKind::Macos)
            },
        );
        assert!(!none.unavailable.contains_key("audio"));
    }

    #[test]
    fn copied_images_are_claimed_only_where_the_watcher_reports_them() {
        use roomler_ai_remote_control::models::OsKind;
        let unwatched = with_honest_words(
            p0c_caps(),
            &HostFacts {
                watches_clipboard_images: false,
                ..p0c_facts(OsKind::Linux)
            },
        );
        assert_eq!(
            unwatched.unavailable["clipboard-image-events"].code,
            "not-watched"
        );
        // Text events and on-request images still work: the words stay.
        assert!(unwatched.clipboard.iter().any(|w| w == "events"));
        assert!(unwatched.clipboard.iter().any(|w| w == "images"));
        assert!(unwatched.supports_clipboard);
        let watched = with_honest_words(p0c_caps(), &p0c_facts(OsKind::Windows));
        assert!(!watched.unavailable.contains_key("clipboard-image-events"));
    }

    #[test]
    fn a_root_daemon_outside_every_session_offers_no_clipboard() {
        use roomler_ai_remote_control::models::OsKind;
        let root = with_honest_words(
            p0c_caps(),
            &HostFacts {
                clipboard_reachable: false,
                watches_clipboard_images: false,
                ..p0c_facts(OsKind::Linux)
            },
        );
        assert!(!root.supports_clipboard, "R10");
        assert!(root.clipboard.is_empty(), "{:?}", root.clipboard);
        assert_eq!(root.unavailable["clipboard"].code, "no-session");
        assert!(
            !root.unavailable.contains_key("clipboard-image-events"),
            "no clipboard at all says so once, not twice"
        );
    }

    /// FR-97 P3e-2 — the viewer offered display match by OS (Windows only);
    /// it follows the device's word now, so a host that cannot switch says
    /// so, and one that can says nothing.
    #[test]
    fn a_host_whose_display_cannot_be_switched_says_so() {
        use roomler_ai_remote_control::models::OsKind;
        // FR-97 P2d-2b — a Mac that can switch says nothing too.
        for os in [OsKind::Windows, OsKind::Linux, OsKind::Macos] {
            let can = with_honest_words(p0c_caps(), &p0c_facts(os));
            assert!(!can.unavailable.contains_key("display-match"), "{os:?}");
        }
        for os in [OsKind::Linux, OsKind::Macos] {
            let cannot = with_honest_words(
                p0c_caps(),
                &HostFacts {
                    display_switchable: false,
                    ..p0c_facts(os)
                },
            );
            let why = &cannot.unavailable["display-match"];
            assert_eq!(why.code, "unsupported", "{os:?}");
            assert!(why.reason.contains("cannot be switched"), "{os:?}: {why:?}");
        }
    }

    #[test]
    fn the_layout_is_offered_only_where_it_can_be_switched() {
        use roomler_ai_remote_control::models::OsKind;
        let with_layout = || AgentCaps {
            layout: vec!["report".into(), "set".into()],
            ..p0c_caps()
        };
        // FR-97 P3e — an X11 session, and every Windows host.
        for os in [OsKind::Linux, OsKind::Windows] {
            let c = with_honest_words(with_layout(), &p0c_facts(os));
            assert_eq!(c.layout, ["report", "set"], "{os:?}");
        }
        // Wayland, or a system install: no chip, and nothing greyed to explain.
        let wayland = with_honest_words(
            with_layout(),
            &HostFacts {
                layout_reachable: false,
                ..p0c_facts(OsKind::Linux)
            },
        );
        assert!(wayland.layout.is_empty(), "{:?}", wayland.layout);
        assert!(!wayland.unavailable.contains_key("layout"));
        // GNOME on Xorg puts its own layout back: the chip, and no picker.
        let gnome = with_honest_words(
            with_layout(),
            &HostFacts {
                layout_switchable: false,
                ..p0c_facts(OsKind::Linux)
            },
        );
        assert_eq!(gnome.layout, ["report"]);
    }

    #[test]
    fn ctrl_alt_del_says_what_the_keys_reach_and_goes_on_a_mac() {
        use roomler_ai_remote_control::models::{OsKind, Unavailable};
        for os in [OsKind::Windows, OsKind::Linux] {
            let c = with_honest_words(p0c_caps(), &p0c_facts(os));
            assert_eq!(c.unavailable["sas"].code, "keys-only", "{os:?}");
        }
        let mac = with_honest_words(p0c_caps(), &p0c_facts(OsKind::Macos));
        assert_eq!(mac.unavailable["sas"].code, Unavailable::NOT_APPLICABLE);
    }

    #[test]
    fn every_reason_uses_a_known_key_and_says_something() {
        use roomler_ai_remote_control::models::{OsKind, Unavailable};
        for os in [OsKind::Windows, OsKind::Macos, OsKind::Linux] {
            for reachable in [true, false] {
                let c = with_honest_words(
                    p0c_caps(),
                    &HostFacts {
                        os,
                        audio_built: true,
                        audio_backend: false,
                        linux_audio_refusal: None,
                        mac_audio_refusal: None,
                        watches_clipboard_images: false,
                        clipboard_reachable: reachable,
                        layout_reachable: reachable,
                        layout_switchable: reachable,
                        display_switchable: reachable,
                    },
                );
                for (key, why) in &c.unavailable {
                    assert!(Unavailable::KEYS.contains(&key.as_str()), "{key}");
                    assert!(
                        !why.code.is_empty() && why.reason.len() > 20,
                        "{key}: {why:?}"
                    );
                }
            }
        }
    }

    /// The facts this build reports for its own OS: what the census will count.
    #[test]
    fn this_hosts_facts_follow_its_os() {
        let f = HostFacts::here();
        if cfg!(target_os = "windows") {
            assert!(f.audio_backend, "WASAPI loopback");
        } else if cfg!(target_os = "linux") {
            // FR-97 P3c: a way to the computer's sound, and a reason exactly
            // when there is none.
            assert_eq!(
                f.audio_backend,
                f.linux_audio_refusal.is_none(),
                "{:?}",
                f.linux_audio_refusal
            );
        } else {
            // FR-97 P2a — ScreenCaptureKit, and a reason exactly when not.
            assert_eq!(
                f.audio_backend,
                f.mac_audio_refusal.is_none(),
                "{:?}",
                f.mac_audio_refusal
            );
        }
        // FR-97 P3e-3a — on Linux, wherever the X11 change gate opens.
        #[cfg(all(target_os = "linux", feature = "clipboard"))]
        assert_eq!(
            f.watches_clipboard_images,
            crate::clipboard::x11_gate::available()
        );
        // FR-97 P2e-1 — on a Mac, wherever its kill switch is on.
        #[cfg(all(target_os = "macos", feature = "clipboard"))]
        assert_eq!(
            f.watches_clipboard_images,
            crate::clipboard::mac_gate::available()
        );
        #[cfg(not(all(any(target_os = "linux", target_os = "macos"), feature = "clipboard")))]
        assert_eq!(f.watches_clipboard_images, cfg!(target_os = "windows"));
        if cfg!(target_os = "windows") {
            assert!(f.layout_reachable && f.layout_switchable);
            assert!(f.display_switchable);
        } else if cfg!(target_os = "macos") {
            // FR-97 P2d-2a — wherever the Mac module offers it.
            #[cfg(all(target_os = "macos", feature = "enigo-input"))]
            assert_eq!(f.layout_reachable, crate::input::layout_mac::offered());
            #[cfg(all(target_os = "macos", not(feature = "enigo-input")))]
            assert!(!f.layout_reachable);
            // FR-97 P2d-2b — wherever its kill switch is on.
            assert_eq!(f.display_switchable, crate::display_match::mac_offered());
        }
    }

    /// #1672 (review of #1679) — the daemon's side of the split, locked. The
    /// two `files_*` tests above prove the child carries nothing and the seam
    /// follows the config; neither proves `detect()` ASSIGNS it. With the
    /// three assignments inline in the `OnceLock` closure, deleting
    /// `caps.files = files_caps()` left every lib test green while every hello
    /// shipped an empty `files` and every browser fell back to upload-only —
    /// and the same latent gap sat under `apps` (#1667) and `rpc`. `detect()`
    /// cannot be called here (it spawns the probe child), so its closure is
    /// the named seam `with_config_lists`, and this drives the exact struct
    /// the driver-free fallback hands it, with the three lists poisoned so an
    /// assignment that is skipped shows as `stale` surviving. Also the home of
    /// the "file transfer survives a probe failure" assertion.
    ///
    /// ⚠️ On Linux `apps_caps()` performs the daemon's own read-only session
    /// discovery (`loginctl`, then `wmctrl -m` per candidate display). The
    /// assertion on `apps` does not depend on what it finds — `status` is a
    /// property of the build — which is what makes calling the real seam,
    /// rather than a copy with the sources injected, the honest choice here.
    #[test]
    fn the_daemon_assigns_every_config_list() {
        let mut base = compute_caps(false, false);
        base.rpc = vec!["stale".into()];
        base.apps = vec!["stale".into()];
        base.files = vec!["stale".into()];
        let caps = with_config_lists(base);

        assert_eq!(
            caps.files,
            files_caps(),
            "`files` must be the daemon's list, assigned over whatever the probe path carried"
        );
        assert!(
            caps.files.iter().any(|f| f == "upload") && caps.files.iter().any(|f| f == "resume"),
            "file transfer must survive a probe failure — this IS the fallback struct: {:?}",
            caps.files
        );
        assert_eq!(
            caps.rpc,
            rpc_caps(),
            "`rpc` must be the daemon's list, assigned over whatever the probe path carried"
        );
        assert!(
            !caps.apps.iter().any(|a| a == "stale"),
            "`apps` must be overwritten by the daemon, never kept from the probe path: {:?}",
            caps.apps
        );
        #[cfg(any(target_os = "linux", target_os = "windows"))]
        assert!(
            caps.apps.iter().any(|a| a == "status"),
            "a build with a Remote Apps backend advertises `status` whatever the host is doing: {:?}",
            caps.apps
        );
    }

    /// FR-56 AC10 (review of #1667) — `apps` is the DAEMON's to assign, in
    /// `detect()`, never the caps-probe child's to compute. On a cache miss
    /// `cached_or_probed` returns the child's struct wholesale, and the child
    /// loads no config: with the apps block inside `compute_caps`, a device
    /// with `[virtual_desktop_apps] enabled = false` still advertised `list`
    /// on every release's first boot and on every boot of a host with no
    /// hardware cell. So what the child runs must leave the field empty — a
    /// re-added block here is exactly the regression, and this is red on it.
    #[test]
    fn apps_are_assigned_by_the_daemon_never_by_the_probe_child() {
        let caps = compute_caps(false, false);
        assert!(
            caps.apps.is_empty(),
            "compute_caps is what the caps-probe child runs; it must leave `apps` to \
             detect(), which reads the installed config in the daemon — got {:?}",
            caps.apps
        );
    }

    /// FR-56 AC10 — `status` is locked on every build with a backend and absent
    /// elsewhere, and the three verbs follow the CONFIG the daemon installed,
    /// not the built-in default. Driven through `apps_caps_for` so no
    /// process-global config is installed and no login session is looked for.
    #[test]
    fn apps_caps_lock_status_and_follow_the_config() {
        use crate::apps::VirtualDesktopAppsConfig;
        let off = VirtualDesktopAppsConfig {
            enabled: false,
            ..Default::default()
        };
        let apps = apps_caps_for(&off);
        let apps: Vec<&str> = apps.iter().map(String::as_str).collect();
        // FR-97 P2c — macOS has a backend too, so it says `status` like the
        // other two.
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        {
            assert_eq!(
                apps,
                vec!["status"],
                "disabled: `status` alone — the entry shows, and the reply says `disabled`"
            );
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        {
            assert!(
                apps.is_empty(),
                "no backend on this platform, nothing to advertise: {apps:?}"
            );
        }
        // Windows always drives the active desktop, so with the feature on the
        // three verbs follow. On Linux the answer depends on a login session,
        // which a unit test must not go looking for.
        #[cfg(target_os = "windows")]
        {
            let on = VirtualDesktopAppsConfig::default();
            let apps = apps_caps_for(&on);
            let apps: Vec<&str> = apps.iter().map(String::as_str).collect();
            assert_eq!(apps, vec!["status", "list", "focus", "launch"]);
        }
    }

    /// The child's output has to survive the round trip, or the parent falls
    /// back on every host and the probe silently stops meaning anything.
    /// FR-77 P3 — the envelope carries the vp9_qsv IDR verdict, and the P1
    /// bare-caps line still parses.
    #[test]
    fn the_marked_json_line_round_trips() {
        let caps = compute_caps(false, false);
        let report = child::ProbeReport {
            caps: caps.clone(),
            vp9_qsv_idr: Some((true, false)),
            gpu_input: vec!["hevc_qsv".into()],
        };
        let line = format!(
            "{}{}",
            child::MARKER,
            serde_json::to_string(&report).unwrap()
        );
        let out = format!("some driver banner\n{line}\n");
        let back = child::parse_line(&out, std::time::Duration::from_millis(1234))
            .expect("the report must parse back");
        assert_eq!(back.caps.codecs, caps.codecs);
        assert_eq!(back.caps.transports, caps.transports);
        assert_eq!(back.caps.hw_encoders, caps.hw_encoders);
        assert_eq!(
            back.caps.video_cells, caps.video_cells,
            "FR-77 cells must survive the round trip"
        );
        assert_eq!(back.vp9_qsv_idr, Some((true, false)));
        assert_eq!(back.gpu_input, vec!["hevc_qsv".to_string()], "FR-118 P2-3");
        assert_eq!(back.caps.probe_ms, Some(1234), "stamped by the parent");

        // A P1-shaped line (bare caps, no envelope) is still a valid answer.
        let bare = format!("{}{}", child::MARKER, serde_json::to_string(&caps).unwrap());
        let back =
            child::parse_line(&bare, std::time::Duration::from_millis(7)).expect("bare caps");
        assert_eq!(back.caps.video_cells, caps.video_cells);
        assert_eq!(back.vp9_qsv_idr, None);
        assert_eq!(back.caps.probe_ms, Some(7));

        assert!(
            child::parse_line("no marker here\n", std::time::Duration::ZERO).is_none(),
            "without the marker there is no answer"
        );
    }

    /// FR-77 P3 — a cache hit takes the encoder matrix from the cache and
    /// NOTHING else: permissions, files, verbs are this process's own.
    #[test]
    fn a_cache_hit_takes_only_the_driver_derived_fields() {
        let mut cached = compute_caps(false, false);
        cached.hw_encoders = vec!["ffmpeg-hevc_nvenc".into()];
        cached.codecs = vec!["h265".into()];
        cached.transports = vec!["data-channel-hevc".into()];
        cached.hevc_chroma = vec!["yuv420".into(), "yuv444".into()];
        cached.video_cells = vec![VideoCell::new(
            VideoCodec::Hevc,
            VideoBackend::Nvenc,
            &[ChromaFormat::Yuv420, ChromaFormat::Yuv444],
            true,
        )];
        // Poison the fields that must NOT come from the cache.
        cached.files = vec!["stale".into()];
        cached.permissions = Some(vec!["stale".into()]);
        cached.rpc = vec!["stale".into()];
        let hit = super::super::caps_cache::CacheFile::new(
            super::super::caps_cache::CacheKey {
                build: "b".into(),
                hardware: "h".into(),
                knobs: "k".into(),
            },
            1,
            3986,
            cached,
            None,
        );
        let merged = merge_cached(hit);
        assert_eq!(merged.hw_encoders, vec!["ffmpeg-hevc_nvenc".to_string()]);
        assert_eq!(merged.transports, vec!["data-channel-hevc".to_string()]);
        assert_eq!(merged.video_cells.len(), 1);
        assert!(merged.video_cells[0].hw);
        assert_eq!(merged.probe_ms, Some(3986));
        assert!(merged.probe_cached);
        assert!(
            !merged.files.iter().any(|f| f == "stale"),
            "files never come from the cache: {:?}",
            merged.files
        );
        // #1672 — and the merge does not compute them either: like `apps`
        // and `rpc`, `detect()` assigns them in the daemon after the merge,
        // so a hit and a miss reach the hello by the same line.
        assert!(
            merged.files.is_empty(),
            "the merge leaves `files` to detect(): {:?}",
            merged.files
        );
        assert_ne!(
            merged.permissions.as_deref(),
            Some(&["stale".to_string()][..])
        );
        assert!(!merged.rpc.iter().any(|r| r == "stale"));
    }

    /// FR-77 P3c — the 4:4:4 phase asks only for hardware cells the base
    /// phase opened whose name the source matrix lists, minus the denylist:
    /// no AV1, no AMF/VideoToolbox/MF, nothing denied, and nothing the host
    /// never opened in 4:2:0.
    #[test]
    fn candidates_444_are_the_hardware_cells_the_matrix_allows_minus_the_denylist() {
        use tunnel_core::env::test_env::Saved;
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _saved = Saved::cleared("ENCODER_CELLS_DENY");
        let caps = AgentCaps {
            video_cells: vec![
                VideoCell::new(
                    VideoCodec::Hevc,
                    VideoBackend::Nvenc,
                    &[ChromaFormat::Yuv420],
                    true,
                ),
                VideoCell::new(
                    VideoCodec::H264,
                    VideoBackend::Nvenc,
                    &[ChromaFormat::Yuv420],
                    true,
                ),
                VideoCell::new(
                    VideoCodec::Av1,
                    VideoBackend::Nvenc,
                    &[ChromaFormat::Yuv420],
                    true,
                ),
                VideoCell::new(
                    VideoCodec::Hevc,
                    VideoBackend::Amf,
                    &[ChromaFormat::Yuv420],
                    true,
                ),
                VideoCell::new(
                    VideoCodec::Vp9,
                    VideoBackend::Qsv,
                    &[ChromaFormat::Yuv420],
                    true,
                ),
                VideoCell::new(
                    VideoCodec::Hevc,
                    VideoBackend::Qsv,
                    &[ChromaFormat::Yuv420],
                    true,
                ),
                VideoCell::new(
                    VideoCodec::Vp9,
                    VideoBackend::Libvpx,
                    &[ChromaFormat::Yuv420],
                    false,
                ),
            ],
            ..AgentCaps::default()
        };
        // Built-in denylist: hevc_qsv, hevc_vaapi AND vp9_qsv (CORPLAP-3, 2026-09-08).
        assert_eq!(
            child::candidates_444(&caps, &denied_cells()),
            vec!["hevc_nvenc".to_string(), "h264_nvenc".to_string()]
        );
        unsafe { tunnel_core::env::test_env::set("ENCODER_CELLS_DENY", "none") };
        assert_eq!(
            child::candidates_444(&caps, &denied_cells()),
            vec!["hevc_nvenc", "h264_nvenc", "vp9_qsv", "hevc_qsv"]
        );
        assert!(child::candidates_444(&AgentCaps::default(), &denied_cells()).is_empty());
    }

    /// FR-118 P2-3, AC6's probe half — the GPU-input phase tries the base
    /// matrix's QSV 4:2:0 cells the GPU input covers, and NOTHING with the
    /// switch off, nor a cell either denylist names: the probe opens only
    /// what a session may (the session's half is `gpu_input`'s
    /// `plan_reads_every_gate_at_the_open`).
    #[cfg(all(
        target_os = "windows",
        feature = "ffmpeg-encoder",
        any(feature = "mf-encoder", feature = "wgc-capture")
    ))]
    #[test]
    fn the_gpu_phase_tries_only_what_a_session_may_open() {
        use tunnel_core::env::test_env::{Saved, set};
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _cells = Saved::cleared("ENCODER_CELLS_DENY");
        let _switch = Saved::cleared("ZERO_COPY");
        let _deny = Saved::cleared("ZERO_COPY_DENY");
        let cell = |codec, backend| VideoCell::new(codec, backend, &[ChromaFormat::Yuv420], true);
        let caps = AgentCaps {
            video_cells: vec![
                cell(VideoCodec::Hevc, VideoBackend::Qsv),
                cell(VideoCodec::Av1, VideoBackend::Qsv),
                cell(VideoCodec::H264, VideoBackend::Qsv),
                cell(VideoCodec::Vp9, VideoBackend::Qsv),
                cell(VideoCodec::Hevc, VideoBackend::Nvenc),
            ],
            ..AgentCaps::default()
        };
        assert!(
            child::candidates_gpu(&caps, &denied_cells()).is_empty(),
            "the switch is off"
        );
        // SAFETY: serialised by DENY_ENV_LOCK; the guards restore all three.
        unsafe { set("ZERO_COPY", "1") };
        assert_eq!(
            child::candidates_gpu(&caps, &denied_cells()),
            vec!["hevc_qsv", "av1_qsv", "h264_qsv", "hevc_nvenc"],
            "vp9_qsv takes no GPU frames yet; NVENC does since P3"
        );
        // SAFETY: as above.
        unsafe { set("ZERO_COPY_DENY", "av1_qsv:gpu") };
        assert_eq!(
            child::candidates_gpu(&caps, &denied_cells()),
            vec!["hevc_qsv", "h264_qsv", "hevc_nvenc"]
        );
        // SAFETY: as above.
        unsafe { set("ENCODER_CELLS_DENY", "h264_qsv:yuv420") };
        assert_eq!(
            child::candidates_gpu(&caps, &denied_cells()),
            vec!["hevc_qsv", "hevc_nvenc"]
        );
        // A list REPLACES the one before it, so both entries go in one list.
        // SAFETY: as above.
        unsafe { set("ZERO_COPY_DENY", "av1_qsv:gpu,hevc_nvenc:gpu") };
        assert_eq!(
            child::candidates_gpu(&caps, &denied_cells()),
            vec!["hevc_qsv"]
        );
    }

    /// FR-118 P5, AC6's probe half on macOS — the GPU-input phase tries
    /// VideoToolbox's 4:2:0 HEVC and H.264 cells, nothing with the switch off,
    /// and no cell either denylist names (the session's half is `vt_input`'s
    /// `plan_reads_every_gate_at_the_open`).
    #[cfg(all(
        target_os = "macos",
        feature = "ffmpeg-encoder",
        feature = "scrap-capture"
    ))]
    #[test]
    fn the_gpu_phase_tries_only_videotoolbox_cells_a_session_may_open() {
        use tunnel_core::env::test_env::{Saved, set};
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _cells = Saved::cleared("ENCODER_CELLS_DENY");
        let _switch = Saved::cleared("ZERO_COPY");
        let _deny = Saved::cleared("ZERO_COPY_DENY");
        let cell = |codec, backend| VideoCell::new(codec, backend, &[ChromaFormat::Yuv420], true);
        let caps = AgentCaps {
            video_cells: vec![
                cell(VideoCodec::Hevc, VideoBackend::VideoToolbox),
                cell(VideoCodec::H264, VideoBackend::VideoToolbox),
                cell(VideoCodec::Av1, VideoBackend::VideoToolbox),
            ],
            ..AgentCaps::default()
        };
        assert!(
            child::candidates_gpu(&caps, &denied_cells()).is_empty(),
            "the switch is off"
        );
        // SAFETY: serialised by DENY_ENV_LOCK; the guards restore all three.
        unsafe { set("ZERO_COPY", "1") };
        assert_eq!(
            child::candidates_gpu(&caps, &denied_cells()),
            vec!["hevc_videotoolbox", "h264_videotoolbox"],
            "AV1 opens on no shipped Mac"
        );
        // SAFETY: as above.
        unsafe { set("ZERO_COPY_DENY", "hevc_videotoolbox:gpu") };
        assert_eq!(
            child::candidates_gpu(&caps, &denied_cells()),
            vec!["h264_videotoolbox"]
        );
        // SAFETY: as above.
        unsafe { set("ZERO_COPY_DENY", "none") };
        // SAFETY: as above.
        unsafe { set("ENCODER_CELLS_DENY", "h264_videotoolbox:yuv420") };
        assert_eq!(
            child::candidates_gpu(&caps, &denied_cells()),
            vec!["hevc_videotoolbox"]
        );
    }

    /// FR-118 P3 — the GPU phase proves NVENC's GPU-input forms on an NVIDIA
    /// device: each opens on the D3D11 pool (not CPU frames) and two GPU frames
    /// come out as packets. Runs where an NVIDIA adapter is (neo16's RTX 5090);
    /// a runner without one has nothing to prove and says so. The test stands
    /// in for the probe child (`ROOMLERD_CAPS_CHILD`), whose job is the proof.
    #[cfg(all(
        target_os = "windows",
        feature = "system-context",
        feature = "mf-encoder",
        feature = "scrap-capture",
        feature = "ffmpeg-encoder"
    ))]
    #[test]
    fn the_gpu_phase_proves_nvenc_on_an_nvidia_device() {
        use tunnel_core::env::test_env::{Saved, set};
        let _guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _switch = Saved::cleared("ZERO_COPY");
        let _deny = Saved::cleared("ZERO_COPY_DENY");
        let _child = Saved::cleared("CAPS_CHILD");
        if crate::capture::gpu_frame::device_on_vendor(0x10de).is_none() {
            eprintln!("no NVIDIA adapter here — nothing to prove");
            return;
        }
        // SAFETY: serialised by DENY_ENV_LOCK; the guards restore all three.
        unsafe {
            set("ZERO_COPY", "1");
            set("CAPS_CHILD", "1");
        }
        let names: Vec<String> = ["hevc_nvenc", "h264_nvenc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let proven = probe_gpu(&names);
        eprintln!("proven on the NVIDIA device: {proven:?}");
        assert_eq!(proven, names, "both NVENC forms take GPU frames");
    }

    /// FR-77 P3c — the merge adds `yuv444` to exactly the named cells, and
    /// the legacy `hevc_chroma` follows the P7 rule (hevc_nvenc won AND opened).
    #[test]
    fn merge_444_marks_the_cells_and_the_legacy_hevc_chroma() {
        let mut report = child::ProbeReport {
            caps: AgentCaps {
                hw_encoders: vec![
                    "openh264-sw".into(),
                    "ffmpeg-hevc_nvenc".into(),
                    "ffmpeg-h264_nvenc".into(),
                ],
                hevc_chroma: vec!["yuv420".into()],
                video_cells: vec![
                    VideoCell::new(
                        VideoCodec::Hevc,
                        VideoBackend::Nvenc,
                        &[ChromaFormat::Yuv420],
                        true,
                    ),
                    VideoCell::new(
                        VideoCodec::H264,
                        VideoBackend::Nvenc,
                        &[ChromaFormat::Yuv420],
                        true,
                    ),
                    VideoCell::new(
                        VideoCodec::Vp9,
                        VideoBackend::Qsv,
                        &[ChromaFormat::Yuv420],
                        true,
                    ),
                ],
                ..AgentCaps::default()
            },
            vp9_qsv_idr: None,
            gpu_input: Vec::new(),
        };
        child::merge_444(
            &mut report,
            &["hevc_nvenc".to_string(), "vp9_qsv".to_string()],
        );
        let cell = |r: &child::ProbeReport, codec: &str, backend: &str| {
            r.caps
                .video_cells
                .iter()
                .find(|c| c.codec == codec && c.backend == backend)
                .unwrap()
                .chroma
                .clone()
        };
        assert_eq!(cell(&report, "hevc", "nvenc"), vec!["yuv420", "yuv444"]);
        assert_eq!(cell(&report, "vp9", "qsv"), vec!["yuv420", "yuv444"]);
        assert_eq!(
            cell(&report, "h264", "nvenc"),
            vec!["yuv420"],
            "not named — untouched"
        );
        assert_eq!(report.caps.hevc_chroma, vec!["yuv420", "yuv444"]);
        // Idempotent.
        child::merge_444(&mut report, &["hevc_nvenc".to_string()]);
        assert_eq!(cell(&report, "hevc", "nvenc"), vec!["yuv420", "yuv444"]);
        assert_eq!(report.caps.hevc_chroma, vec!["yuv420", "yuv444"]);
        // A QSV HEVC winner never makes the legacy field say 4:4:4 (the P7
        // session path is hevc_nvenc's).
        let mut qsv = child::ProbeReport {
            caps: AgentCaps {
                hw_encoders: vec!["ffmpeg-hevc_qsv".into()],
                hevc_chroma: vec!["yuv420".into()],
                video_cells: vec![VideoCell::new(
                    VideoCodec::Hevc,
                    VideoBackend::Qsv,
                    &[ChromaFormat::Yuv420],
                    true,
                )],
                ..AgentCaps::default()
            },
            vp9_qsv_idr: None,
            gpu_input: Vec::new(),
        };
        child::merge_444(&mut qsv, &["hevc_qsv".to_string()]);
        assert_eq!(qsv.caps.hevc_chroma, vec!["yuv420"]);
        assert_eq!(qsv.caps.video_cells[0].chroma, vec!["yuv420", "yuv444"]);
    }

    /// FR-77 P3c — the 4:4:4 phase's answer and the progress trail parse; a
    /// crashed child leaves the name of the open it was inside.
    #[test]
    fn the_444_phase_lines_and_the_progress_trail_parse() {
        let out = format!(
            "{}hevc_nvenc:yuv444\nsome driver banner\n{}h264_nvenc:yuv444\n{}[\"hevc_nvenc\"]\n",
            child::PROGRESS,
            child::PROGRESS,
            child::MARKER_444
        );
        assert_eq!(
            child::parse_444_line(&out),
            Some(vec!["hevc_nvenc".to_string()])
        );
        assert_eq!(
            child::last_progress(&out).as_deref(),
            Some("h264_nvenc:yuv444")
        );
        let died = format!("{}vp9_qsv:yuv444\n", child::PROGRESS);
        assert_eq!(child::parse_444_line(&died), None);
        assert_eq!(
            child::last_progress(&died).as_deref(),
            Some("vp9_qsv:yuv444")
        );
        assert_eq!(child::last_progress(""), None);
    }

    /// #3267 — a child that dies between opens did not die in the last cell it
    /// opened: the window closed. `h264_vulkan` opening fine and the process
    /// dying in its next enumeration must not cost `h264_vulkan`.
    #[test]
    fn a_closed_window_names_no_cell() {
        let open = format!(
            "{p}hevc_nvenc:yuv420\n{p}{no}\n{p}hevc_vulkan:yuv420\n",
            p = child::PROGRESS,
            no = child::NO_OPEN
        );
        assert_eq!(
            child::last_progress(&open).as_deref(),
            Some("hevc_vulkan:yuv420")
        );
        let closed = format!(
            "{p}hevc_vulkan:yuv420\n{p}{no}\n",
            p = child::PROGRESS,
            no = child::NO_OPEN
        );
        assert_eq!(child::last_progress(&closed), None);
    }

    /// Outside a probe child an announced open is only the open: this process
    /// is no child, and `compute_caps(true, _)` runs in tests like this.
    #[test]
    fn an_announced_open_returns_what_the_open_returned() {
        assert_eq!(
            announced_open("hevc_nvenc", ChromaFormat::Yuv420, || 7),
            7,
            "the announcement must not get in the way of the open"
        );
    }

    fn base_died_in(cell: &str, status: &str, fault: bool) -> child::Run {
        child::Run {
            ending: child::Ending::Died {
                status: status.to_string(),
                fault,
            },
            stdout: format!("{}{cell}\n", child::PROGRESS),
            cut_off: false,
            elapsed: std::time::Duration::from_secs(2),
        }
    }

    fn base_hung_in(cell: &str) -> child::Run {
        child::Run {
            ending: child::Ending::Hung { gone: true },
            stdout: format!("{}{cell}\n", child::PROGRESS),
            cut_off: false,
            elapsed: std::time::Duration::from_secs(60),
        }
    }

    /// #3273 — the kill did not land.
    fn base_stuck_in(cell: &str) -> child::Run {
        child::Run {
            ending: child::Ending::Hung { gone: false },
            stdout: format!("{}{cell}\n", child::PROGRESS),
            cut_off: true,
            elapsed: std::time::Duration::from_secs(70),
        }
    }

    fn base_died_blind(status: &str) -> child::Run {
        child::Run {
            ending: child::Ending::Died {
                status: status.to_string(),
                fault: false,
            },
            stdout: String::new(),
            cut_off: false,
            elapsed: std::time::Duration::from_secs(2),
        }
    }

    fn base_report_line() -> String {
        let report = child::ProbeReport {
            caps: compute_caps(false, false),
            vp9_qsv_idr: None,
            gpu_input: Vec::new(),
        };
        format!(
            "{}{}\n",
            child::MARKER,
            serde_json::to_string(&report).unwrap()
        )
    }

    fn base_reported() -> child::Run {
        child::Run {
            ending: child::Ending::Clean,
            stdout: base_report_line(),
            cut_off: false,
            elapsed: std::time::Duration::from_secs(4),
        }
    }

    /// What a scripted sequence of base runs came to: whether a report came
    /// out, the cells learned, and the deny list each run was started with.
    struct Scripted {
        reported: bool,
        learned: Vec<crate::encode::cells::LearnedDeny>,
        asked: Vec<Vec<String>>,
        out_of_time: bool,
        stopped: bool,
    }

    /// `script` is the runs in order; each `Duration` is what the clock reads
    /// when that run has ended. Running out of script fails the test: the
    /// loop asked for a run the case says it must not make.
    fn run_base_scripted(
        learning: bool,
        script: Vec<(std::time::Duration, Option<child::Run>)>,
    ) -> Scripted {
        let clock = std::cell::Cell::new(std::time::Duration::ZERO);
        let asked = std::cell::RefCell::new(Vec::new());
        let mut script = script.into_iter();
        let out = child::run_base(
            learning,
            Vec::new(),
            child::BASE_RETRY_BUDGET,
            |denied| {
                asked
                    .borrow_mut()
                    .push(denied.iter().map(|d| d.cell.clone()).collect::<Vec<_>>());
                let (at, run) = script.next().expect("a run the case does not expect");
                clock.set(at);
                run
            },
            || clock.get(),
        );
        Scripted {
            reported: out.report.is_some(),
            learned: out.denied,
            asked: asked.into_inner(),
            out_of_time: out.out_of_time,
            stopped: out.stopped,
        }
    }

    fn secs(s: u64) -> std::time::Duration {
        std::time::Duration::from_secs(s)
    }

    const NOTHING: &[&str] = &[];

    /// #3267, the field's own case: the same cell faults twice. The second run
    /// adds nothing, the third runs without the cell, and the host gets its
    /// report. Only the third run may deny it, and the first says nothing yet.
    #[test]
    fn two_faults_in_one_cell_cost_that_cell_and_not_the_host() {
        let out = run_base_scripted(
            true,
            vec![
                (
                    secs(2),
                    Some(base_died_in("hevc_vulkan:yuv420", "exit code: 255", true)),
                ),
                (
                    secs(4),
                    Some(base_died_in(
                        "hevc_vulkan:yuv420",
                        "exit code: 0xc0000005",
                        true,
                    )),
                ),
                (secs(9), Some(base_reported())),
            ],
        );
        assert!(out.reported);
        assert_eq!(
            out.asked,
            vec![
                NOTHING.to_vec(),
                NOTHING.to_vec(),
                vec!["hevc_vulkan:yuv420"]
            ]
        );
        assert_eq!(
            out.learned,
            vec![crate::encode::cells::LearnedDeny {
                cell: "hevc_vulkan:yuv420".into(),
                why: "exit code: 0xc0000005".into(),
                confirmed: true,
                timed_out: false,
            }],
            "two faults confirm it: this is what the cache keeps"
        );
    }

    /// #3267 — a transient must not cost a healthy cell: one death, a clean
    /// second run with nothing denied, and the matrix is whole.
    #[test]
    fn one_fault_then_a_clean_run_learns_nothing() {
        let out = run_base_scripted(
            true,
            vec![
                (
                    secs(2),
                    Some(base_died_in(
                        "hevc_nvenc:yuv420",
                        "exit code: 0xc0000005",
                        true,
                    )),
                ),
                (secs(6), Some(base_reported())),
            ],
        );
        assert!(out.reported);
        assert_eq!(out.asked, vec![NOTHING.to_vec(), NOTHING.to_vec()]);
        assert!(out.learned.is_empty(), "{:?}", out.learned);
    }

    /// #3267 — a hang earns this process only, and gets no confirming run: a
    /// second minute is too much to prove what load alone can do. The next run
    /// skips the cell, and `confirmed: false` keeps the answer out of the cache.
    #[test]
    fn a_hang_is_denied_for_this_process_without_a_second_hang() {
        let out = run_base_scripted(
            true,
            vec![
                (secs(60), Some(base_hung_in("hevc_nvenc:yuv420"))),
                (secs(65), Some(base_reported())),
            ],
        );
        assert!(out.reported);
        assert_eq!(out.asked, vec![NOTHING.to_vec(), vec!["hevc_nvenc:yuv420"]]);
        assert_eq!(out.learned.len(), 1);
        assert!(
            !out.learned[0].confirmed,
            "a hang proves too little to keep"
        );
        assert_eq!(out.learned[0].why, "hung");
    }

    /// #3267 — an OOM kill or an EDR's terminate that lands in the same open
    /// twice protects this process's sessions and proves nothing to keep. So
    /// does a fault one run and a kill the next.
    #[test]
    fn a_kill_never_confirms_a_cell() {
        for (first_fault, second_fault) in [(false, false), (true, false), (false, true)] {
            let out = run_base_scripted(
                true,
                vec![
                    (
                        secs(2),
                        Some(base_died_in(
                            "av1_nvenc:yuv420",
                            "signal: 9 (SIGKILL)",
                            first_fault,
                        )),
                    ),
                    (
                        secs(4),
                        Some(base_died_in(
                            "av1_nvenc:yuv420",
                            "signal: 9 (SIGKILL)",
                            second_fault,
                        )),
                    ),
                    (secs(8), Some(base_reported())),
                ],
            );
            assert!(out.reported);
            assert_eq!(out.learned.len(), 1);
            assert!(!out.learned[0].confirmed, "{first_fault} {second_fault}");
        }
    }

    /// #3267 — evidence is one cell twice IN A ROW. Two different cells are two
    /// transients until one of them repeats.
    #[test]
    fn a_death_elsewhere_clears_the_suspicion() {
        let out = run_base_scripted(
            true,
            vec![
                (
                    secs(2),
                    Some(base_died_in("hevc_nvenc:yuv420", "exit code: 255", true)),
                ),
                (
                    secs(4),
                    Some(base_died_in("hevc_amf:yuv420", "exit code: 255", true)),
                ),
                (secs(8), Some(base_reported())),
            ],
        );
        assert!(out.reported);
        assert_eq!(
            out.asked,
            vec![NOTHING.to_vec(), NOTHING.to_vec(), NOTHING.to_vec()]
        );
        assert!(out.learned.is_empty(), "{:?}", out.learned);
    }

    /// #3267 — Intel's runtime and AMD's driver can both be on one host: each
    /// cell pays for itself, and every later run carries the list so far.
    #[test]
    fn two_faulting_cells_are_learned_one_after_the_other() {
        let out = run_base_scripted(
            true,
            vec![
                (
                    secs(2),
                    Some(base_died_in("hevc_qsv:yuv420", "exit code: 255", true)),
                ),
                (
                    secs(4),
                    Some(base_died_in("hevc_qsv:yuv420", "exit code: 255", true)),
                ),
                (
                    secs(6),
                    Some(base_died_in("hevc_vulkan:yuv420", "exit code: 255", true)),
                ),
                (
                    secs(8),
                    Some(base_died_in("hevc_vulkan:yuv420", "exit code: 255", true)),
                ),
                (secs(12), Some(base_reported())),
            ],
        );
        assert!(out.reported);
        assert_eq!(
            out.asked,
            vec![
                NOTHING.to_vec(),
                NOTHING.to_vec(),
                vec!["hevc_qsv:yuv420"],
                vec!["hevc_qsv:yuv420"],
                vec!["hevc_qsv:yuv420", "hevc_vulkan:yuv420"],
            ]
        );
        assert!(out.learned.iter().all(|l| l.confirmed));
    }

    /// #3267 — no open in progress, no cell to blame: one more run for a
    /// transient (desktop heap, an EDR at start), and the second such death is
    /// the old answer, with nothing learned.
    #[test]
    fn a_death_with_no_cell_costs_one_run_and_then_the_old_answer() {
        let out = run_base_scripted(
            true,
            vec![
                (secs(1), Some(base_died_blind("exit code: 0xc0000142"))),
                (secs(2), Some(base_died_blind("exit code: 0xc0000142"))),
            ],
        );
        assert!(!out.reported, "no report, no hardware: as before #3267");
        assert!(out.learned.is_empty());
        assert_eq!(out.asked.len(), 2);

        let out = run_base_scripted(
            true,
            vec![
                (secs(1), Some(base_died_blind("exit code: 0xc0000142"))),
                (secs(5), Some(base_reported())),
            ],
        );
        assert!(out.reported, "a transient: the retry's report stands");
        assert!(out.learned.is_empty());
    }

    /// #3267 — the bound on runs: a host that dies somewhere new each time
    /// stops at the limit, however quickly it dies.
    #[test]
    fn the_runs_stop_at_the_limit() {
        let script = (0..child::MAX_BASE_RUNS)
            .map(|i| {
                (
                    secs(u64::from(i)),
                    Some(base_died_in(
                        &format!("cell{i}:yuv420"),
                        "exit code: 255",
                        true,
                    )),
                )
            })
            .collect();
        let out = run_base_scripted(true, script);
        assert!(!out.reported);
        assert!(!out.out_of_time, "a kill says nothing about time");
        assert_eq!(out.asked.len() as u32, child::MAX_BASE_RUNS);
        assert!(out.learned.is_empty(), "no cell died twice: nothing earned");
    }

    /// #3267 — the bound on time: no run starts past the budget, and what the
    /// runs learned still stands, because a session opens in the daemon.
    #[test]
    fn no_run_starts_past_the_time_budget_and_the_cell_stays_denied() {
        let over = child::BASE_RETRY_BUDGET + secs(1);
        let out = run_base_scripted(
            true,
            vec![
                (
                    secs(40),
                    Some(base_died_in("hevc_vulkan:yuv420", "exit code: 255", true)),
                ),
                (
                    over,
                    Some(base_died_in("hevc_vulkan:yuv420", "exit code: 255", true)),
                ),
            ],
        );
        assert!(!out.reported);
        assert!(out.out_of_time, "that is what a later try is for");
        assert_eq!(out.asked.len(), 2, "the script would panic on a third");
        assert_eq!(out.learned.len(), 1);
        assert!(out.learned[0].confirmed);
    }

    /// #3267 — the base phase's worst case, as the PR states it: one last
    /// start just inside the budget, running its whole timeout.
    #[test]
    fn the_worst_case_is_the_budget_plus_one_timeout() {
        assert_eq!(child::BASE_RETRY_BUDGET, secs(90));
        let out = run_base_scripted(
            true,
            vec![
                (
                    secs(89),
                    Some(base_died_in("hevc_nvenc:yuv420", "exit code: 255", true)),
                ),
                (secs(149), Some(base_hung_in("h264_nvenc:yuv420"))),
            ],
        );
        assert!(!out.reported);
        assert_eq!(out.asked.len(), 2);
    }

    /// #3267 — a child that dies in a cell it was told not to open means the
    /// list is not being honoured, and another run cannot help.
    #[test]
    fn a_death_in_a_denied_cell_stops_the_runs() {
        let out = run_base_scripted(
            true,
            vec![
                (
                    secs(2),
                    Some(base_died_in("hevc_vulkan:yuv420", "exit code: 255", true)),
                ),
                (
                    secs(4),
                    Some(base_died_in("hevc_vulkan:yuv420", "exit code: 255", true)),
                ),
                (
                    secs(6),
                    Some(base_died_in("hevc_vulkan:yuv420", "exit code: 255", true)),
                ),
            ],
        );
        assert!(!out.reported);
        assert_eq!(out.asked.len(), 3);
        assert_eq!(out.learned.len(), 1);
    }

    /// #3267 — the report is the answer however the child ended afterwards:
    /// every open behind it had come back. A driver's DLL teardown faulting on
    /// the way out has no cell to cost, and no reason to cost the host.
    #[test]
    fn a_report_stands_when_the_child_dies_after_printing_it() {
        let out = run_base_scripted(
            true,
            vec![(
                secs(5),
                Some(child::Run {
                    ending: child::Ending::Died {
                        status: "exit code: 0xc0000005".into(),
                        fault: true,
                    },
                    stdout: format!(
                        "{p}h264_nvenc:yuv420\n{p}{no}\n{line}",
                        p = child::PROGRESS,
                        no = child::NO_OPEN,
                        line = base_report_line()
                    ),
                    cut_off: false,
                    elapsed: secs(5),
                }),
            )],
        );
        assert!(out.reported);
        assert_eq!(out.asked.len(), 1);
        assert!(out.learned.is_empty());
    }

    /// #3267 — a driver that calls `exit(0)` inside an open leaves a clean
    /// status and no report. That is a death, not an answer.
    #[test]
    fn a_clean_exit_with_no_report_is_a_death() {
        let exited = || child::Run {
            ending: child::Ending::Clean,
            stdout: format!("{}vp9_qsv:yuv420\n", child::PROGRESS),
            cut_off: false,
            elapsed: secs(2),
        };
        let out = run_base_scripted(
            true,
            vec![
                (secs(2), Some(exited())),
                (secs(4), Some(exited())),
                (secs(8), Some(base_reported())),
            ],
        );
        assert!(out.reported);
        assert_eq!(out.learned.len(), 1);
        assert!(!out.learned[0].confirmed);

        // With no open in progress nothing died: it just has no answer, and a
        // second run would have none either. Every integration test's probe
        // ends this way, its test binary spawned as `caps-probe`.
        let out = run_base_scripted(
            true,
            vec![(
                secs(1),
                Some(child::Run {
                    ending: child::Ending::Clean,
                    stdout: "running 0 tests\n".to_string(),
                    cut_off: false,
                    elapsed: secs(1),
                }),
            )],
        );
        assert!(!out.reported);
        assert_eq!(out.asked.len(), 1);
    }

    /// #3267 — a child that could not be started will not start on a second
    /// try this second either: no retry, and no hardware as before.
    #[test]
    fn a_child_that_cannot_start_stops_the_runs() {
        let out = run_base_scripted(true, vec![(secs(0), None)]);
        assert!(!out.reported);
        assert_eq!(out.asked.len(), 1);
    }

    /// #3267 — the kill switch is the behaviour before it: one run, and a child
    /// that does not exit cleanly means no hardware, whatever it printed.
    #[test]
    fn learning_off_is_one_run_and_a_death_means_no_hardware() {
        let out = run_base_scripted(
            false,
            vec![(
                secs(2),
                Some(base_died_in("hevc_vulkan:yuv420", "exit code: 255", true)),
            )],
        );
        assert!(!out.reported);
        assert_eq!(out.asked.len(), 1);
        assert!(out.learned.is_empty());

        let mut died_after_report = base_reported();
        died_after_report.ending = child::Ending::Died {
            status: "exit code: 255".into(),
            fault: true,
        };
        let out = run_base_scripted(false, vec![(secs(2), Some(died_after_report))]);
        assert!(!out.reported);

        let out = run_base_scripted(false, vec![(secs(4), Some(base_reported()))]);
        assert!(out.reported, "a healthy host never sees the switch");
    }

    /// #3267 — the statuses that can confirm a cell, on the real type. A status
    /// that reads as a fault on the wrong OS, or a kill that reads as a fault,
    /// is how a healthy cell gets cached out.
    #[cfg(windows)]
    #[test]
    fn a_windows_fault_is_an_exception_code_or_the_255_the_field_saw() {
        use std::os::windows::process::ExitStatusExt;
        for code in [
            0xC000_0005u32, // access violation: the AMD fault
            0xC000_001D,    // illegal instruction
            0xC000_0094,    // integer division by zero
            0xC000_00FD,    // stack overflow
            0xC000_0374,    // heap corruption
            0xC000_0409,    // fast fail, what `abort` leaves
            0xC000_0602,    // fail fast
            0xE06D_7363,    // an uncaught C++ exception
            255,            // 2 of the 7 AMD deaths read this
        ] {
            assert!(
                child::is_fault(&std::process::ExitStatus::from_raw(code)),
                "{code:#x}"
            );
        }
        for code in [
            0u32,
            1,           // `Child::kill` and many a TerminateProcess
            2,           // the child's own "could not serialise"
            3,           // EXIT_PARENT_GONE
            101,         // a Rust panic
            0xC000_013A, // Ctrl+C
            0xC000_0017, // out of memory
            0xC000_012D, // out of commit
            0xC000_0022, // the image was refused
            0xC000_0135, // a DLL is missing
            0xC000_0142, // DLL init failed: desktop heap
            0xC000_0006, // a page could not be read: the disk, or memory
            0xC000_009A, // out of resources
            0xC000_026B, // the session is logging off
            0xC000_0906, // an antivirus's verdict
            0xC000_0001, // an error nobody has named: an unknown code proves nothing
            0xFFFF_FFFF, // `Stop-Process`, and whatever else terminates with -1
            0x4000_0000, // not an error's severity
        ] {
            assert!(
                !child::is_fault(&std::process::ExitStatus::from_raw(code)),
                "{code:#x}"
            );
        }
    }

    /// #3267 — the same on Unix: a signal a driver raises on itself, and never
    /// the OOM killer's or an operator's.
    #[cfg(unix)]
    #[test]
    fn a_unix_fault_is_a_signal_a_driver_raises_on_itself() {
        use std::os::unix::process::ExitStatusExt;
        for signal in [
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGILL,
            libc::SIGFPE,
            libc::SIGABRT,
        ] {
            assert!(
                child::is_fault(&std::process::ExitStatus::from_raw(signal)),
                "{signal}"
            );
        }
        for signal in [libc::SIGKILL, libc::SIGTERM, libc::SIGINT, libc::SIGPIPE] {
            assert!(
                !child::is_fault(&std::process::ExitStatus::from_raw(signal)),
                "{signal}"
            );
        }
        // A plain exit is no signal, whatever its code.
        for code in [1, 2, 3, 101, 255] {
            assert!(!child::is_fault(&std::process::ExitStatus::from_raw(
                code << 8
            )));
        }
    }

    /// #3267 — the host `base` gave up on keeps what needs no driver. libvpx is
    /// software, so losing it to a probe that died in a vendor's driver drops
    /// VP9 4:4:4 over the DataChannel for nothing. And nothing else comes with
    /// it: no hardware cell, no candidate for the 4:4:4 or GPU children, and so
    /// nothing the cache would keep.
    #[test]
    fn giving_up_on_the_hardware_keeps_libvpx_and_nothing_else() {
        let report = software_report();
        let caps = &report.caps;
        assert_eq!(
            caps.transports.iter().any(|t| t == "data-channel-vp9-444"),
            cfg!(feature = "vp9-444"),
            "{:?}",
            caps.transports
        );
        assert_eq!(
            caps.hw_encoders.iter().any(|h| h == "libvpx-vp9-444-sw"),
            cfg!(feature = "vp9-444"),
            "{:?}",
            caps.hw_encoders
        );
        assert_eq!(
            caps.video_cells
                .iter()
                .any(|c| c.backend == VideoBackend::Libvpx.wire()),
            cfg!(feature = "vp9-444"),
            "{:?}",
            caps.video_cells
        );
        assert_eq!(
            !caps.vp9_chroma.is_empty(),
            cfg!(feature = "vp9-444"),
            "the viewer configures its decoder from this"
        );
        assert!(
            caps.video_cells.iter().all(|c| !c.hw),
            "a hardware cell with no driver asked: {:?}",
            caps.video_cells
        );
        assert!(
            !caps
                .hw_encoders
                .iter()
                .any(|h| h.starts_with("ffmpeg-") || h.starts_with("mf-")),
            "{:?}",
            caps.hw_encoders
        );
        assert!(child::candidates_444(caps, &denied_cells()).is_empty());
        assert!(child::candidates_gpu(caps, &denied_cells()).is_empty());
        assert!(
            !super::super::caps_cache::worth_caching(caps),
            "and the next start tests the hardware again"
        );
        assert!(report.vp9_qsv_idr.is_none() && report.gpu_input.is_empty());
    }

    /// #3267 — a learned deny reaches a fleet read, by the one seam every path
    /// to the hello goes through, under a key every reader already skips. No
    /// list, no word: a healthy host's `unavailable` does not change.
    #[test]
    fn a_learned_cell_reaches_the_hello_as_an_honest_word() {
        use roomler_ai_remote_control::models::{OsKind, Unavailable};
        let learned = vec![crate::encode::cells::LearnedDeny {
            cell: "hevc_vulkan:yuv420".into(),
            why: "exit code: 0xc0000005".into(),
            confirmed: true,
            timed_out: false,
        }];
        let c = assigned_by_the_daemon(p0c_caps(), &p0c_facts(OsKind::Windows), &learned, None);
        let word = &c.unavailable[LEARNED_WORD];
        assert_eq!(word.code, LEARNED_CODE);
        assert!(
            word.reason.contains("hevc_vulkan:yuv420") && word.reason.contains("0xc0000005"),
            "the reason names the cell and why: {}",
            word.reason
        );
        assert!(
            Unavailable::KEYS.contains(&LEARNED_WORD),
            "the wire lock names it"
        );
        assert_ne!(
            word.code,
            Unavailable::NOT_APPLICABLE,
            "the one code a viewer acts on"
        );
        // The seam still assigns what it always did.
        assert!(c.unavailable.contains_key("sas"));
        assert!(!c.files.is_empty(), "with_config_lists still runs");

        // A cell denied on a hang comes back at a later try or the next start,
        // whatever day that is, and the word says so.
        let hung = vec![crate::encode::cells::LearnedDeny {
            cell: "hevc_nvenc:yuv420".into(),
            why: "hung".into(),
            confirmed: false,
            timed_out: true,
        }];
        let short = child::Shortfall {
            cells: vec!["hevc_nvenc:yuv420".into()],
            ..Default::default()
        };
        let c =
            assigned_by_the_daemon(p0c_caps(), &p0c_facts(OsKind::Windows), &hung, Some(&short));
        assert!(
            c.unavailable[LEARNED_WORD]
                .reason
                .contains("hevc_nvenc:yuv420 (hung, until a later try or the next start)"),
            "{}",
            c.unavailable[LEARNED_WORD].reason
        );

        // With the kill switch off no later try comes, and the word does not say
        // one will: `detect` passes no shortfall then.
        let c = assigned_by_the_daemon(p0c_caps(), &p0c_facts(OsKind::Windows), &hung, None);
        assert!(
            c.unavailable[LEARNED_WORD]
                .reason
                .contains("hevc_nvenc:yuv420 (hung, until the next start)"),
            "{}",
            c.unavailable[LEARNED_WORD].reason
        );

        // A double kill is not tried again: only the next start tests it.
        let killed = vec![crate::encode::cells::LearnedDeny {
            cell: "hevc_nvenc:yuv420".into(),
            why: "signal: 9".into(),
            confirmed: false,
            timed_out: false,
        }];
        let c = assigned_by_the_daemon(p0c_caps(), &p0c_facts(OsKind::Windows), &killed, None);
        assert!(
            c.unavailable[LEARNED_WORD]
                .reason
                .contains("hevc_nvenc:yuv420 (signal: 9, until the next start)"),
            "{}",
            c.unavailable[LEARNED_WORD].reason
        );

        let none = assigned_by_the_daemon(p0c_caps(), &p0c_facts(OsKind::Windows), &[], None);
        assert!(!none.unavailable.contains_key(LEARNED_WORD));
    }

    /// #2719 — a line written to a pipe whose reader is gone (the parent was
    /// terminated mid-probe) reads as "the parent is gone", where `println!`
    /// panicked. A real OS pipe, so the error is the one the field saw:
    /// `os error 232` on Windows, EPIPE elsewhere.
    #[test]
    fn a_line_to_a_closed_pipe_reads_as_the_parent_gone() {
        let line = format!("{}{{}}", child::MARKER);
        let (reader, mut writer) = std::io::pipe().expect("an OS pipe");
        drop(reader);
        let e = child::write_line(&mut writer, &line)
            .expect_err("nobody reads the pipe, so the write must fail");
        assert!(
            child::parent_gone(&e),
            "a closed pipe must read as the parent gone: {e:?} (kind {:?}, os error {:?})",
            e.kind(),
            e.raw_os_error()
        );
        // Any other failure is still an error worth a line in the log.
        assert!(!child::parent_gone(&std::io::Error::other("disk full")));

        // A reader that is there gets the line, newline-terminated, as the
        // parent's marker parse expects.
        let (mut reader, mut writer) = std::io::pipe().expect("an OS pipe");
        child::write_line(&mut writer, &line).expect("a live reader");
        drop(writer);
        let mut got = String::new();
        std::io::Read::read_to_string(&mut reader, &mut got).expect("read the pipe");
        assert_eq!(got, format!("{line}\n"));
    }

    /// FR-77 P3c — the child reads its phase from the environment; unset is
    /// the pre-P3c single-phase probe.
    #[test]
    fn the_phase_is_read_from_the_environment() {
        // These env vars are private to the probe child, so the plain
        // std::env API is the contract (no ROOMLERD_ prefix bridging).
        // SAFETY: no other test in this crate touches ROOMLERD_CAPS_PHASE.
        unsafe { std::env::remove_var(child::PHASE_ENV) };
        assert_eq!(child::phase_from_env(), child::Phase::Full);
        unsafe { std::env::set_var(child::PHASE_ENV, "base") };
        assert_eq!(child::phase_from_env(), child::Phase::Base);
        unsafe { std::env::set_var(child::PHASE_ENV, "software") };
        assert_eq!(child::phase_from_env(), child::Phase::Software);
        unsafe { std::env::set_var(child::PHASE_ENV, "444") };
        assert_eq!(child::phase_from_env(), child::Phase::Only444);
        unsafe { std::env::set_var(child::NAMES_444_ENV, " hevc_nvenc, vp9_qsv ,") };
        assert_eq!(child::names_444_from_env(), vec!["hevc_nvenc", "vp9_qsv"]);
        unsafe {
            std::env::remove_var(child::PHASE_ENV);
            std::env::remove_var(child::NAMES_444_ENV);
        }
    }

    /// #2717 — a probe child tied to this process ends with its job's last
    /// handle. Dropping the job here is what the process's exit does to it
    /// when a worker leaves a probe behind.
    #[cfg(windows)]
    #[test]
    fn a_probe_child_ends_when_its_job_closes() {
        use std::process::Stdio;
        use std::time::{Duration, Instant};
        let mut probe = std::process::Command::new("cmd")
            .args(["/c", "ping -n 30 127.0.0.1 > nul"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a stand-in probe");
        let job = child::tie_to_this_process(&probe).expect("the probe joins a job");

        // The control: while the job is open, the probe runs.
        std::thread::sleep(Duration::from_millis(300));
        let ran_on = probe.try_wait().expect("try_wait").is_none();

        drop(job);
        let deadline = Instant::now() + Duration::from_secs(10);
        let ended = loop {
            if probe.try_wait().expect("try_wait").is_some() {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        if !ended {
            let _ = probe.kill();
            let _ = probe.wait();
        }
        assert!(ran_on, "the control: the probe runs while its job is open");
        assert!(ended, "the probe outlived its job's last handle");
    }

    /// #3273 — a stand-in for a probe child: a script in the platform's shell,
    /// with stdout piped as `run_child` pipes it. The second value ends it with
    /// the test on Windows, where a job can; elsewhere its `sleep` runs out.
    fn stand_in(windows: &str, unix: &str) -> (std::process::Child, Box<dyn std::any::Any>) {
        use std::process::{Command, Stdio};
        let mut cmd = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/c", windows]);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", unix]);
            cmd
        };
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a stand-in probe child");
        #[cfg(windows)]
        let job: Box<dyn std::any::Any> = Box::new(child::tie_to_this_process(&child));
        #[cfg(not(windows))]
        let job: Box<dyn std::any::Any> = Box::new(());
        (child, job)
    }

    /// #3273 — every stand-in below stays this long, or leaves something behind
    /// holding its pipe for this long. Anything that waits for it fails [`STAND_IN_HELD`].
    const STAND_IN_WIN: &str = "ping -n 16 127.0.0.1 > nul";
    const STAND_IN_UNIX: &str = "sleep 15";
    const STAND_IN_HELD: std::time::Duration = std::time::Duration::from_secs(12);
    /// What a stand-in that has said its line gets before it counts as hung.
    const STAND_IN_HANG: std::time::Duration = std::time::Duration::from_secs(4);
    const STAND_IN_KILL: std::time::Duration = std::time::Duration::from_secs(3);
    const STAND_IN_CELL: &str = "hevc_vulkan:yuv420";

    /// #3273 — nothing can make a real process ignore a kill, so this one is
    /// told nothing: to the parent that is what a child stuck in a kernel call
    /// looks like. It is alive and it holds its pipe, as that child would.
    #[test]
    fn a_child_that_does_not_die_when_killed_does_not_hold_the_probe() {
        let (child, _job) = stand_in(
            &format!("echo {}{STAND_IN_CELL}& {STAND_IN_WIN}", child::PROGRESS),
            &format!("echo {}{STAND_IN_CELL}; {STAND_IN_UNIX}", child::PROGRESS),
        );
        let started = std::time::Instant::now();
        let (waited, stdout) =
            child::wait_for_child(child, STAND_IN_HANG, STAND_IN_KILL, &|| false, |_| {});
        assert!(
            started.elapsed() < STAND_IN_HELD,
            "waited for the child: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(waited, child::Waited::Hung { gone: false }),
            "{waited:?}"
        );
        assert_eq!(
            child::last_progress(&stdout.text).as_deref(),
            Some(STAND_IN_CELL),
            "the line that lets a stuck child cost one cell and not the host: {stdout:?}"
        );
        assert!(stdout.cut_off, "the child still holds its pipe: {stdout:?}");
        // #3280 — a later try waits on this. It holds until that call returns, and
        // other tests here may be holding it too, so this looks for the end.
        assert!(child::stuck_children() >= 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while child::stuck_children() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        assert_eq!(child::stuck_children(), 0, "the reaper counts it off");
    }

    /// #3273 — the kill lands, and what the child started keeps the pipe open.
    #[test]
    fn a_killed_child_whose_pipe_lives_on_does_not_hold_the_probe() {
        let (child, _job) = stand_in(
            &format!("echo {}{STAND_IN_CELL}& {STAND_IN_WIN}", child::PROGRESS),
            &format!(
                "echo {}{STAND_IN_CELL}; {STAND_IN_UNIX} & wait",
                child::PROGRESS
            ),
        );
        let started = std::time::Instant::now();
        let (waited, stdout) =
            child::wait_for_child(child, STAND_IN_HANG, STAND_IN_KILL, &|| false, |c| {
                let _ = c.kill();
            });
        assert!(
            started.elapsed() < STAND_IN_HELD,
            "waited for the pipe: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(waited, child::Waited::Hung { gone: true }),
            "{waited:?}"
        );
        assert_eq!(
            child::last_progress(&stdout.text).as_deref(),
            Some(STAND_IN_CELL),
            "{stdout:?}"
        );
    }

    /// #3273 — the child ends by itself, its report written, and something it
    /// started holds the pipe. `TIMEOUT` never bounded this wait.
    #[test]
    fn a_child_that_exits_while_its_pipe_lives_on_keeps_what_it_wrote() {
        let (child, _job) = stand_in(
            &format!(
                "echo {}{STAND_IN_CELL}& start /b {STAND_IN_WIN}",
                child::PROGRESS
            ),
            &format!(
                "echo {}{STAND_IN_CELL}; {STAND_IN_UNIX} & exit 0",
                child::PROGRESS
            ),
        );
        let started = std::time::Instant::now();
        let (waited, stdout) = child::wait_for_child(
            child,
            std::time::Duration::from_secs(30),
            STAND_IN_KILL,
            &|| false,
            |_| panic!("a child that ended by itself is never killed"),
        );
        assert!(
            started.elapsed() < STAND_IN_HELD,
            "waited for the pipe: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(&waited, child::Waited::Exited(s) if s.success()),
            "{waited:?}"
        );
        assert_eq!(
            child::last_progress(&stdout.text).as_deref(),
            Some(STAND_IN_CELL),
            "{stdout:?}"
        );
    }

    /// #3273 — the path every host takes: the pipe ends with the child, so the
    /// bound is never waited out, and every line arrives.
    #[test]
    fn a_child_that_ends_cleanly_pays_nothing_for_the_bound() {
        let (child, _job) = stand_in("echo one& echo two& exit 3", "echo one; echo two; exit 3");
        let limit = std::time::Duration::from_secs(30);
        let started = std::time::Instant::now();
        let (waited, stdout) = child::wait_for_child(child, limit, limit, &|| false, |_| {
            panic!("a child that ended by itself is never killed")
        });
        assert!(
            started.elapsed() < STAND_IN_HELD,
            "waited out the bound: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(&waited, child::Waited::Exited(s) if s.code() == Some(3)),
            "{waited:?}"
        );
        assert_eq!(
            stdout.text.lines().map(str::trim).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert!(!stdout.cut_off, "the pipe ended with the child");
    }

    /// #3273 — `read_to_string` dropped everything on one bad byte, and a report
    /// lost that way reads as no hardware.
    #[test]
    fn a_byte_that_is_not_utf8_costs_that_byte_and_not_the_rest() {
        let (chunks, output) = std::sync::mpsc::channel();
        chunks
            .send((
                std::time::Instant::now(),
                b"\xffbanner\nROOMLER_CAPS_PROGRESS:hevc_nvenc:yuv420\n".to_vec(),
            ))
            .unwrap();
        drop(chunks);
        let stdout = child::collect_output(output, std::time::Instant::now());
        assert_eq!(
            child::last_progress(&stdout.text).as_deref(),
            Some("hevc_nvenc:yuv420"),
            "{stdout:?}"
        );
    }

    /// #3273 — half a line names a cell that does not exist, so it is dropped
    /// while the pipe is still open. A pipe that ended has no more of it coming,
    /// so there what is left stays.
    #[test]
    fn a_pipe_still_open_gives_back_whole_lines_and_an_ended_one_everything() {
        let line = b"ROOMLER_CAPS_PROGRESS:hevc_nvenc:yuv420\nROOMLER_CAPS_PROGRESS:hevc_vul";

        let (chunks, output) = std::sync::mpsc::channel();
        chunks
            .send((std::time::Instant::now(), line.to_vec()))
            .unwrap();
        let stdout = child::collect_output(output, std::time::Instant::now());
        drop(chunks);
        assert_eq!(
            child::last_progress(&stdout.text).as_deref(),
            Some("hevc_nvenc:yuv420"),
            "{stdout:?}"
        );

        let (chunks, output) = std::sync::mpsc::channel();
        chunks
            .send((std::time::Instant::now(), line.to_vec()))
            .unwrap();
        drop(chunks);
        let stdout = child::collect_output(output, std::time::Instant::now());
        assert_eq!(
            child::last_progress(&stdout.text).as_deref(),
            Some("hevc_vul"),
            "{stdout:?}"
        );
    }

    /// #3273 — the review's flood: chunks that keep coming never let the wait
    /// time out, so the arm that takes one has to read the clock too, or
    /// "at most `timeout` plus `kill_wait`, whatever the child does" is untrue.
    #[test]
    fn a_child_that_floods_its_pipe_does_not_outlast_the_deadline() {
        let deadline = std::time::Instant::now() + secs(60);
        let (chunks, output) = std::sync::mpsc::channel();
        chunks
            .send((deadline - secs(1), b"in time\n".to_vec()))
            .unwrap();
        for after in [1, 2, 3] {
            chunks
                .send((deadline + secs(after), b"too late\n".to_vec()))
                .unwrap();
        }
        let got = child::collect_output(output, deadline);
        assert!(got.cut_off, "{got:?}");
        assert_eq!(got.text, "in time\n");
    }

    /// #3273 — a stuck child's lines are queued before the deadline, and the kill
    /// spends the whole of it, so this starts with none left. Every line counts:
    /// reading only the first would deny the first cell the child opened.
    #[test]
    fn a_stuck_child_still_gives_back_every_line_it_wrote_in_time() {
        let (chunks, output) = std::sync::mpsc::channel();
        for cell in ["h264_nvenc:yuv420", "-", "hevc_nvenc:yuv420"] {
            chunks
                .send((
                    std::time::Instant::now(),
                    format!("{}{cell}\n", child::PROGRESS).into_bytes(),
                ))
                .unwrap();
        }
        let got = child::collect_output(output, std::time::Instant::now());
        assert!(got.cut_off, "the pipe has not ended: {got:?}");
        assert_eq!(
            child::last_progress(&got.text).as_deref(),
            Some("hevc_nvenc:yuv420"),
            "{got:?}"
        );
        drop(chunks);
    }

    /// #3273 — the review's medium: a `444` or `gpu` child that ran out of time
    /// leaves a 4:2:0 answer with those forms missing for want of an answer.
    /// Stored, it is a cache hit for a week that never tests them again.
    #[test]
    fn an_answer_short_of_time_is_not_frozen_in_the_cache() {
        let mut caps = compute_caps(false, false);
        caps.video_cells.push(VideoCell::new(
            VideoCodec::H264,
            VideoBackend::Nvenc,
            &[ChromaFormat::Yuv420],
            true,
        ));
        assert_eq!(why_not_cached(&caps, &[], false), None, "the plain case");
        assert!(why_not_cached(&caps, &[], true).is_some());
    }

    /// #3273 — what counts as out of time. A fault's death is an answer about
    /// the host and stays cached, as it always did.
    #[test]
    fn only_a_hang_or_a_read_cut_off_counts_as_out_of_time() {
        let run = |ending, cut_off| child::Run {
            ending,
            stdout: String::new(),
            cut_off,
            elapsed: std::time::Duration::from_secs(1),
        };
        let fault = || child::Ending::Died {
            status: "exit code: 0xc0000005".to_string(),
            fault: true,
        };
        assert!(child::ran_out_of_time(&run(
            child::Ending::Hung { gone: true },
            false
        )));
        assert!(child::ran_out_of_time(&run(
            child::Ending::Hung { gone: false },
            true
        )));
        assert!(child::ran_out_of_time(&run(child::Ending::Clean, true)));
        assert!(!child::ran_out_of_time(&run(child::Ending::Clean, false)));
        assert!(!child::ran_out_of_time(&run(fault(), false)));
        assert!(!child::ran_out_of_time(&run(
            child::Ending::Stopped { gone: true },
            false
        )));
    }

    /// #3273 — the review's question: a host that comes online with no hardware
    /// still runs Media Foundation's cascade in the daemon at its first session
    /// (`encode::open_default`), on a wait with no bound. A child that would not
    /// die is the evidence that it could hang there, so those cells go, for this
    /// process only, and that session takes OpenH264.
    #[test]
    fn a_child_that_would_not_die_keeps_the_daemon_out_of_media_foundation() {
        let mf = child::MF_IN_THE_DAEMON
            .iter()
            .map(|n| format!("{n}:yuv420"))
            .collect::<Vec<_>>();
        let out = run_base_scripted(
            true,
            vec![
                (secs(70), Some(base_stuck_in("hevc_nvenc:yuv420"))),
                (secs(74), Some(base_reported())),
            ],
        );
        assert!(out.reported);
        let mut want = vec!["hevc_nvenc:yuv420".to_string()];
        want.extend(mf);
        assert_eq!(
            out.learned
                .iter()
                .map(|d| d.cell.clone())
                .collect::<Vec<_>>(),
            want
        );
        assert!(out.learned.iter().all(|d| !d.confirmed), "never cached");
        assert_eq!(out.asked[1], want, "the next run is told too");
        assert_eq!(
            cfg!(windows),
            !child::MF_IN_THE_DAEMON.is_empty(),
            "Media Foundation is Windows only"
        );
    }

    /// #3273 — a hang whose kill landed proves far less, and a merely slow host
    /// keeps its Media Foundation session.
    #[test]
    fn a_hang_whose_kill_landed_leaves_media_foundation_alone() {
        let out = run_base_scripted(
            true,
            vec![
                (secs(60), Some(base_hung_in("hevc_nvenc:yuv420"))),
                (secs(64), Some(base_reported())),
            ],
        );
        assert!(out.reported);
        assert_eq!(
            out.learned
                .iter()
                .map(|d| d.cell.clone())
                .collect::<Vec<_>>(),
            ["hevc_nvenc:yuv420"]
        );
    }

    /// #3273 — stuck in an MF cell itself: denied once, and no run is told it
    /// died in a cell it was told not to open.
    #[test]
    fn a_child_stuck_in_a_media_foundation_cell_is_not_denied_twice() {
        let cell = format!("{}:yuv420", crate::encode::cells::MF_H264);
        let out = run_base_scripted(
            true,
            vec![
                (secs(70), Some(base_stuck_in(&cell))),
                (secs(74), Some(base_reported())),
            ],
        );
        assert!(out.reported);
        assert_eq!(
            out.learned.iter().filter(|d| d.cell == cell).count(),
            1,
            "{:?}",
            out.learned
        );
    }

    /// #3280 — a session asked: the runs stop where they are, with nothing to say
    /// about time, and no second run.
    #[test]
    fn a_stop_ends_the_runs_at_once() {
        let out = run_base_scripted(
            true,
            vec![(
                secs(1),
                Some(child::Run {
                    ending: child::Ending::Stopped { gone: true },
                    stdout: String::new(),
                    cut_off: false,
                    elapsed: secs(1),
                }),
            )],
        );
        assert!(out.stopped);
        assert!(!out.reported && !out.out_of_time);
        assert!(out.learned.is_empty());
    }

    /// #3273 — two children stuck with no cell to blame: the old answer, no
    /// hardware, and the daemon stays out of Media Foundation all the same. This
    /// is the state the review asked about.
    #[test]
    fn two_stuck_children_with_no_cell_to_blame_keep_the_daemon_out_too() {
        let blind = || child::Run {
            ending: child::Ending::Hung { gone: false },
            stdout: String::new(),
            cut_off: true,
            elapsed: secs(70),
        };
        let out = run_base_scripted(
            true,
            vec![(secs(70), Some(blind())), (secs(140), Some(blind()))],
        );
        assert!(!out.reported);
        assert!(out.out_of_time);
        assert_eq!(
            out.learned
                .iter()
                .map(|d| d.cell.clone())
                .collect::<Vec<_>>(),
            child::MF_IN_THE_DAEMON
                .iter()
                .map(|n| format!("{n}:yuv420"))
                .collect::<Vec<_>>()
        );
    }

    // #3280 — a host that is merely slow gets another try by itself.

    use crate::encode::cells::{self, LearnedDeny};

    /// What every test of a whole probe holds: the list and the denylists are
    /// process-wide, and a probe joins a fault's deny to the list at once. The
    /// environment comes back while the lock is still held.
    struct ProbeFixture {
        _saved: Vec<tunnel_core::env::test_env::Saved>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for ProbeFixture {
        fn drop(&mut self) {
            cells::install_learned(Vec::new());
        }
    }

    fn probe_fixture() -> ProbeFixture {
        use tunnel_core::env::test_env::{Saved, set};
        let guard = crate::encode::DENY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let saved = [
            "ENCODER_CELLS_DENY",
            "ENCODER_CELLS_LEARNED",
            "ZERO_COPY",
            "ZERO_COPY_DENY",
            "CAPS_PROBE_RETRY",
        ]
        .map(Saved::cleared)
        .into_iter()
        .collect();
        // SAFETY: serialised by DENY_ENV_LOCK; the guards restore it.
        unsafe { set("ZERO_COPY", "0") };
        cells::install_learned(Vec::new());
        ProbeFixture {
            _saved: saved,
            _guard: guard,
        }
    }

    fn a_first_probe() -> child::Try<'static> {
        child::Try {
            bounds: child::Bounds::FIRST,
            denied: Vec::new(),
            stop: &|| false,
            first: true,
        }
    }

    fn a_later_try(denied: Vec<LearnedDeny>) -> child::Try<'static> {
        child::Try {
            bounds: LADDER[0].bounds,
            denied,
            stop: &|| false,
            first: false,
        }
    }

    /// A whole probe over scripted children: `script` is each child in order, by
    /// its phase. Asking for one the case does not expect fails it, and so does
    /// leaving one out. Also returns the list each child was started with.
    fn probe_scripted(
        this: &child::Try,
        script: Vec<(&'static str, Option<child::Run>)>,
    ) -> (child::Probed, Vec<Vec<String>>) {
        let mut script = script.into_iter();
        let mut asked = Vec::new();
        let got = child::probe_with(this, true, |list, extra| {
            asked.push(list.iter().map(|d| d.cell.clone()).collect::<Vec<_>>());
            let phase = extra
                .iter()
                .find(|(k, _)| *k == child::PHASE_ENV)
                .map(|(_, v)| v.clone())
                .expect("every child has a phase");
            let (want, run) = script
                .next()
                .unwrap_or_else(|| panic!("a {phase} child the case does not expect"));
            assert_eq!(phase, want);
            run
        });
        assert!(
            script.next().is_none(),
            "a child the case expected did not run"
        );
        (got, asked)
    }

    /// An answer with one 4:4:4-capable hardware cell, on top of what needs no
    /// driver, so a `444` child follows.
    fn nvenc_caps() -> AgentCaps {
        let mut caps = compute_caps(false, false);
        caps.video_cells.push(VideoCell::new(
            VideoCodec::Hevc,
            VideoBackend::Nvenc,
            &[ChromaFormat::Yuv420],
            true,
        ));
        caps
    }

    fn ran_clean(stdout: String) -> child::Run {
        child::Run {
            ending: child::Ending::Clean,
            stdout,
            cut_off: false,
            elapsed: secs(4),
        }
    }

    fn report_of(caps: AgentCaps) -> child::Run {
        let report = child::ProbeReport {
            caps,
            vp9_qsv_idr: None,
            gpu_input: Vec::new(),
        };
        ran_clean(format!(
            "{}{}\n",
            child::MARKER,
            serde_json::to_string(&report).unwrap()
        ))
    }

    fn answer_444(names: &[&str]) -> child::Run {
        ran_clean(format!(
            "{}{}\n",
            child::MARKER_444,
            serde_json::to_string(names).unwrap()
        ))
    }

    fn hung_blind() -> child::Run {
        child::Run {
            ending: child::Ending::Hung { gone: true },
            stdout: String::new(),
            cut_off: false,
            elapsed: secs(60),
        }
    }

    fn hung_in_444() -> child::Run {
        base_hung_in("hevc_nvenc:yuv444")
    }

    fn mf_cells() -> Vec<String> {
        child::MF_IN_THE_DAEMON
            .iter()
            .map(|n| format!("{n}:yuv420"))
            .collect()
    }

    fn names(list: &[LearnedDeny]) -> Vec<String> {
        list.iter().map(|d| d.cell.clone()).collect()
    }

    /// The table's first row: a report from every child it asked. The next probe
    /// has nothing to add, so this arms nothing.
    #[test]
    fn a_report_from_every_child_it_asked_is_an_answer() {
        let _fixture = probe_fixture();
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(answer_444(&["hevc_nvenc"]))),
            ],
        );
        assert_eq!(got.short, None);
        assert!(!got.unanswered && !got.stopped);
        let report = got.report.expect("a report");
        assert!(
            forms_of(&report.caps, &report.gpu_input).contains("hevc_nvenc:yuv444"),
            "the 4:4:4 child's answer is folded in"
        );
    }

    /// A blind hang costs one run and no cell, so the report after it is complete.
    #[test]
    fn a_blind_hang_and_then_a_report_lost_nothing() {
        let _fixture = probe_fixture();
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(hung_blind())),
                ("base", Some(base_reported())),
            ],
        );
        assert!(got.report.is_some());
        assert_eq!(got.short, None);
    }

    /// The pipe ended and nothing was on it, so time has nothing to do with it. A
    /// test binary spawned as `caps-probe` ends this way in every integration test,
    /// so this also says those arm nothing.
    #[test]
    fn a_clean_exit_with_no_report_is_not_time() {
        let _fixture = probe_fixture();
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(ran_clean(String::new()))),
                ("software", Some(ran_clean(String::new()))),
            ],
        );
        assert!(got.report.is_none());
        assert_eq!(got.short, None);
    }

    /// A fault is an answer: learned, joined to the daemon's list at once, and kept.
    #[test]
    fn a_fault_twice_in_one_cell_is_an_answer() {
        let _fixture = probe_fixture();
        let cell = "hevc_vulkan:yuv420";
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                (
                    "base",
                    Some(base_died_in(cell, "exit code: 0xc0000005", true)),
                ),
                (
                    "base",
                    Some(base_died_in(cell, "exit code: 0xc0000005", true)),
                ),
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(answer_444(&[]))),
            ],
        );
        assert_eq!(got.short, None);
        assert_eq!(names(&got.learned), [cell]);
        assert!(got.learned[0].confirmed && !got.learned[0].timed_out);
        assert_eq!(names(&cells::learned()), [cell]);
        let report = got.report.expect("a report");
        assert_eq!(
            why_not_cached(&report.caps, &got.learned, got.unanswered),
            None,
            "this is what the cache keeps"
        );
    }

    /// A kill twice, named or blind, proves too little to keep, and more time would
    /// not change it.
    #[test]
    fn any_other_death_twice_is_not_time() {
        let _fixture = probe_fixture();
        let cell = "hevc_nvenc:yuv420";
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(base_died_in(cell, "signal: 9", false))),
                ("base", Some(base_died_in(cell, "signal: 9", false))),
                ("base", Some(base_reported())),
            ],
        );
        assert_eq!(got.short, None);
        assert_eq!(names(&got.learned), [cell]);
        assert!(!got.learned[0].confirmed && !got.learned[0].timed_out);

        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(base_died_blind("signal: 9"))),
                ("base", Some(base_died_blind("signal: 9"))),
                ("software", Some(base_reported())),
            ],
        );
        assert!(got.report.is_some());
        assert_eq!(got.short, None);
    }

    #[test]
    fn a_child_that_could_not_be_spawned_is_not_time() {
        let _fixture = probe_fixture();
        let (got, _) = probe_scripted(&a_first_probe(), vec![("base", None), ("software", None)]);
        assert!(got.report.is_none());
        assert_eq!(got.short, None);
    }

    #[test]
    fn a_hang_in_a_named_cell_is_short_by_that_cell() {
        let _fixture = probe_fixture();
        let cell = "hevc_nvenc:yuv420";
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(base_hung_in(cell))),
                ("base", Some(base_reported())),
            ],
        );
        assert!(got.report.is_some());
        assert_eq!(
            got.short,
            Some(child::Shortfall {
                base: false,
                cells: vec![cell.to_string()],
                forms: false,
            })
        );
        assert!(got.learned[0].timed_out && !got.learned[0].confirmed);
    }

    /// `base` gave up, and time is why: its runs hung, or one was cut off, or it
    /// spent its budget. What stands is the software child's answer.
    #[test]
    fn base_giving_up_for_want_of_time_is_short_of_everything() {
        let _fixture = probe_fixture();
        let short = |got: &child::Probed| got.short.as_ref().is_some_and(|s| s.base);

        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(hung_blind())),
                ("base", Some(hung_blind())),
                ("software", Some(base_reported())),
            ],
        );
        assert!(got.report.is_some() && short(&got), "two blind hangs");

        let cut_off = child::Run {
            cut_off: true,
            ..ran_clean(String::new())
        };
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![("base", Some(cut_off)), ("software", Some(base_reported()))],
        );
        assert!(short(&got), "a report lost to the bound on the read");

        // Past its budget with nothing hung: the first death is already too late.
        let spent = child::Try {
            bounds: child::Bounds {
                base_budget: Duration::ZERO,
                ..child::Bounds::FIRST
            },
            ..a_first_probe()
        };
        let (got, _) = probe_scripted(
            &spent,
            vec![
                (
                    "base",
                    Some(base_died_in("hevc_nvenc:yuv420", "signal: 9", false)),
                ),
                ("software", Some(base_reported())),
            ],
        );
        assert!(short(&got), "the budget ran out");

        // A later try leaves the answer in force alone: no software child.
        let (got, _) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![("base", Some(hung_blind())), ("base", Some(hung_blind()))],
        );
        assert!(got.report.is_none() && short(&got));
    }

    /// A `444` child that ran out of time costs those forms, and they are worth
    /// another try. The report stands.
    #[test]
    fn a_444_child_out_of_time_is_short_by_those_forms() {
        let _fixture = probe_fixture();
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(hung_in_444())),
            ],
        );
        assert!(got.report.is_some());
        assert!(got.unanswered, "and it must not be cached");
        assert_eq!(
            got.short,
            Some(child::Shortfall {
                base: false,
                cells: vec!["hevc_nvenc:yuv444".to_string()],
                forms: true,
            })
        );

        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(report_of(nvenc_caps()))),
                (
                    "444",
                    Some(child::Run {
                        cut_off: true,
                        ..ran_clean(String::new())
                    }),
                ),
            ],
        );
        assert!(got.unanswered);
        assert_eq!(
            got.short,
            Some(child::Shortfall {
                base: false,
                cells: Vec::new(),
                forms: true,
            }),
            "cut off, with no form to name"
        );
    }

    /// The same four lines for the `gpu` child, where a build can reach them.
    #[cfg(all(
        target_os = "windows",
        feature = "ffmpeg-encoder",
        any(feature = "mf-encoder", feature = "wgc-capture")
    ))]
    #[test]
    fn a_gpu_child_out_of_time_is_short_by_those_forms() {
        use tunnel_core::env::test_env::set;
        let _fixture = probe_fixture();
        // SAFETY: serialised by DENY_ENV_LOCK; the fixture restores it.
        unsafe { set("ZERO_COPY", "1") };
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(answer_444(&[]))),
                ("gpu", Some(base_hung_in("hevc_nvenc:gpu"))),
            ],
        );
        assert!(got.unanswered);
        assert_eq!(
            got.short,
            Some(child::Shortfall {
                base: false,
                cells: vec!["hevc_nvenc:gpu".to_string()],
                forms: true,
            })
        );

        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(answer_444(&[]))),
                (
                    "gpu",
                    Some(child::Run {
                        ending: child::Ending::Hung { gone: true },
                        ..ran_clean(format!("{}[\"hevc_nvenc\"]\n", child::MARKER_GPU))
                    }),
                ),
            ],
        );
        assert_eq!(got.short, None, "it printed its answer before it hung");
        assert!(!got.unanswered);
        assert_eq!(got.report.expect("a report").gpu_input, ["hevc_nvenc"]);
    }

    /// The second review's gap: an OOM kill, an EDR's terminate or a failed spawn
    /// says nothing about the host, so it must not freeze the 4:2:0 answer for a
    /// week. A fault's death does, and costs the forms it always cost. None of
    /// these is short of time, so none is worth a try.
    #[test]
    fn only_a_faults_death_lets_a_444_gap_into_the_cache() {
        let _fixture = probe_fixture();
        let run_444 = |run: Option<child::Run>| {
            probe_scripted(
                &a_first_probe(),
                vec![("base", Some(report_of(nvenc_caps()))), ("444", run)],
            )
            .0
        };
        let got = run_444(Some(base_died_in(
            "hevc_nvenc:yuv444",
            "exit code: 0xc0000005",
            true,
        )));
        assert!(!got.unanswered && got.short.is_none());

        for lost in [
            Some(base_died_in("hevc_nvenc:yuv444", "signal: 9", false)),
            Some(ran_clean(String::new())),
            None,
        ] {
            let got = run_444(lost);
            assert!(got.unanswered);
            assert_eq!(got.short, None);
            let report = got.report.expect("the 4:2:0 matrix stands");
            assert!(why_not_cached(&report.caps, &got.learned, got.unanswered).is_some());
        }
    }

    /// The second review's gap: `base` honours a report from a child that then ended
    /// badly, and so do these. A child that prints its answer and hangs in its exit
    /// is the one way a host could never cache.
    #[test]
    fn a_444_answer_stands_however_the_child_ended() {
        let _fixture = probe_fixture();
        for ending in [
            child::Ending::Hung { gone: true },
            child::Ending::Died {
                status: "exit code: 0xc0000005".to_string(),
                fault: true,
            },
        ] {
            let (got, _) = probe_scripted(
                &a_first_probe(),
                vec![
                    ("base", Some(report_of(nvenc_caps()))),
                    (
                        "444",
                        Some(child::Run {
                            ending,
                            ..answer_444(&["hevc_nvenc"])
                        }),
                    ),
                ],
            );
            assert_eq!(got.short, None);
            assert!(!got.unanswered);
            let report = got.report.expect("a report");
            assert!(forms_of(&report.caps, &[]).contains("hevc_nvenc:yuv444"));
        }

        // Learning off is what came before: only a clean exit's word counts.
        let (got, _) = {
            let mut script = vec![
                report_of(nvenc_caps()),
                child::Run {
                    ending: child::Ending::Hung { gone: true },
                    ..answer_444(&["hevc_nvenc"])
                },
            ]
            .into_iter();
            let this = a_first_probe();
            (child::probe_with(&this, false, |_, _| script.next()), ())
        };
        let report = got.report.expect("a report");
        assert!(!forms_of(&report.caps, &[]).contains("hevc_nvenc:yuv444"));
    }

    /// The second review's gap: the deny fired only from a `base` child that left
    /// no report. A driver stuck in the kernel is as stuck whichever child found it.
    #[test]
    fn a_child_stuck_anywhere_keeps_the_daemon_out_of_media_foundation() {
        let _fixture = probe_fixture();
        let stuck = |stdout: String| child::Run {
            ending: child::Ending::Hung { gone: false },
            stdout,
            cut_off: true,
            elapsed: secs(70),
        };

        // `base`, after it reported: the report stands.
        let reported = report_of(compute_caps(false, false)).stdout;
        let (got, _) = probe_scripted(&a_first_probe(), vec![("base", Some(stuck(reported)))]);
        assert!(got.report.is_some());
        assert_eq!(names(&got.learned), mf_cells());

        // A `444` child.
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(stuck(String::new()))),
            ],
        );
        assert_eq!(names(&got.learned), mf_cells());

        // The software child.
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(ran_clean(String::new()))),
                ("software", Some(stuck(String::new()))),
            ],
        );
        assert_eq!(names(&got.learned), mf_cells());
    }

    /// A child stuck after the report: the answer stops offering what a session is
    /// now refused, or an RTP session would bind to a codec nothing can open. A
    /// codec another cell still offers stays.
    #[cfg(windows)]
    #[test]
    fn a_child_stuck_after_the_report_withdraws_media_foundation() {
        let _fixture = probe_fixture();
        let mut caps = nvenc_caps();
        caps.video_cells.push(VideoCell::new(
            VideoCodec::Av1,
            VideoBackend::MediaFoundation,
            &[ChromaFormat::Yuv420],
            true,
        ));
        caps.video_cells.push(VideoCell::new(
            VideoCodec::Hevc,
            VideoBackend::MediaFoundation,
            &[ChromaFormat::Yuv420],
            true,
        ));
        caps.codecs = vec!["h265".to_string(), "av1".to_string()];
        caps.hw_encoders = vec!["mf-av1-hw".to_string(), "ffmpeg-hevc_nvenc".to_string()];
        let (got, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(report_of(caps))),
                ("444", Some(base_stuck_in("hevc_nvenc:yuv444"))),
            ],
        );
        let report = got.report.expect("the report stands");
        assert!(
            !report
                .caps
                .video_cells
                .iter()
                .any(|c| c.backend == VideoBackend::MediaFoundation.wire()),
            "{:?}",
            report.caps.video_cells
        );
        assert_eq!(report.caps.codecs, ["h265"], "FFmpeg's cell keeps HEVC");
        assert_eq!(report.caps.hw_encoders, ["ffmpeg-hevc_nvenc"]);
    }

    /// A session asked: the probe ends where it is, and it starts nothing further.
    #[test]
    fn a_session_stops_a_probe_where_it_is() {
        let _fixture = probe_fixture();
        let stopped = |gone| child::Run {
            ending: child::Ending::Stopped { gone },
            stdout: String::new(),
            cut_off: false,
            elapsed: secs(1),
        };
        let (got, _) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![("base", Some(stopped(true)))],
        );
        assert!(got.stopped && got.report.is_none() && got.short.is_none());
        assert!(got.learned.is_empty());

        let (got, _) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(stopped(true))),
            ],
        );
        assert!(
            got.stopped && got.report.is_none(),
            "half a try is no answer"
        );

        // One that would not die is still stuck, and the session is on its way.
        let (got, _) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![("base", Some(stopped(false)))],
        );
        assert!(got.stopped);
        assert_eq!(names(&got.learned), mf_cells());
    }

    /// The wait itself: `stop` ends it, and the child is told to die.
    #[test]
    fn a_stop_ends_the_wait_and_the_child() {
        let (child, _job) = stand_in(STAND_IN_WIN, STAND_IN_UNIX);
        let started = std::time::Instant::now();
        let stop = || started.elapsed() > std::time::Duration::from_millis(300);
        let (waited, _) = child::wait_for_child(
            child,
            std::time::Duration::from_secs(60),
            STAND_IN_KILL,
            &stop,
            |c| {
                let _ = c.kill();
            },
        );
        assert!(
            matches!(waited, child::Waited::Stopped { gone: true }),
            "{waited:?}"
        );
        assert!(
            started.elapsed() < STAND_IN_HELD,
            "waited for the child: {:?}",
            started.elapsed()
        );
    }

    /// What holds a try back, and stops the table's clock.
    #[test]
    fn a_leftover_child_or_a_session_holds_a_try_back() {
        assert!(quiet(0, false, None));
        assert!(!quiet(1, false, None), "a child is still stuck");
        assert!(!quiet(0, true, None), "a session is live");
        assert!(!quiet(0, false, Some(secs(59))), "a session just asked");
        assert!(quiet(0, false, Some(secs(60))));
    }

    /// The clock stops, and a session that just ended still owes a try a minute,
    /// even when it asked long ago.
    #[test]
    fn a_try_owes_a_minute_after_anything_that_held_it_back() {
        assert_eq!(after_a_look(secs(600), true), secs(599));
        assert_eq!(after_a_look(secs(1), true), secs(0));
        assert_eq!(after_a_look(secs(600), false), secs(600), "the clock stops");
        assert_eq!(
            after_a_look(secs(1), false),
            QUIET_AFTER_ASK,
            "a try a session stopped owes nothing else, so this is its whole wait"
        );
        // A long session asks once, then ends: the look after it counts the minute
        // from there, though `quiet` is already true.
        let mut owed = secs(1);
        owed = after_a_look(owed, quiet(0, true, Some(secs(3600))));
        for _ in 0..59 {
            owed = after_a_look(owed, quiet(0, false, Some(secs(3600))));
            assert!(!owed.is_zero());
        }
        owed = after_a_look(owed, quiet(0, false, Some(secs(3600))));
        assert!(owed.is_zero());
    }

    /// A later try's own stuck child ends it: only one child is scripted in each
    /// case, so a second one fails it. It reads as short of time, because what is
    /// missing is missing for want of it.
    #[test]
    fn a_later_try_starts_nothing_after_its_own_child_stuck() {
        let _fixture = probe_fixture();
        let stuck = |stdout: String| child::Run {
            ending: child::Ending::Hung { gone: false },
            stdout,
            cut_off: false,
            elapsed: secs(120),
        };

        let (got, asked) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![("base", Some(stuck(String::new())))],
        );
        assert_eq!(asked.len(), 1);
        assert!(got.report.is_none() && !got.stopped);
        let short = got.short.expect("a stuck child is out of time");
        assert!(short.base);
        assert_eq!(short.cells, mf_cells());

        // Its report stands, as a first probe's does, and no `444` child follows it.
        let printed = report_of(nvenc_caps()).stdout;
        let (got, asked) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![("base", Some(stuck(printed)))],
        );
        assert_eq!(asked.len(), 1);
        assert!(got.report.is_some());
        assert_eq!(names(&got.learned), mf_cells());

        // A first probe has nothing to fall back on, so it goes on to ask.
        let (got, asked) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(stuck(String::new()))),
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(answer_444(&[]))),
            ],
        );
        assert_eq!(asked.len(), 3);
        assert!(got.report.is_some());
    }

    /// A deny for want of time comes off a try's list, and nothing else does.
    #[test]
    fn a_faults_deny_stays_on_every_trys_list() {
        let deny = |cell: &str, confirmed, timed_out| LearnedDeny {
            cell: cell.to_string(),
            why: "x".to_string(),
            confirmed,
            timed_out,
        };
        let list = vec![
            deny("a_fault:yuv420", true, false),
            deny("a_double_kill:yuv420", false, false),
            deny("a_hang:yuv420", false, true),
            deny("h264_mf:yuv420", false, true),
        ];
        assert_eq!(
            names(&list_for_a_try(&list)),
            ["a_fault:yuv420", "a_double_kill:yuv420"]
        );
    }

    /// A fault a try finds is in force at once, whatever becomes of that try.
    #[test]
    fn a_fault_found_by_a_try_joins_the_daemons_list_at_once() {
        let _fixture = probe_fixture();
        let cell = "hevc_nvenc:yuv420";
        let (got, _) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![
                (
                    "base",
                    Some(base_died_in(cell, "exit code: 0xc0000005", true)),
                ),
                (
                    "base",
                    Some(base_died_in(cell, "exit code: 0xc0000005", true)),
                ),
                ("base", Some(hung_blind())),
                ("base", Some(hung_blind())),
            ],
        );
        assert!(got.report.is_none(), "this try has no answer to adopt");
        assert_eq!(names(&cells::learned()), [cell]);
        assert!(
            cell_denied(&denied_cells(), "hevc_nvenc", ChromaFormat::Yuv420),
            "a session cannot open it now"
        );
        assert_eq!(names(&list_for_a_try(&cells::learned())), [cell]);
    }

    /// AC5 — the Media Foundation deny comes back through the retry: off the next
    /// try's list, still on the daemon's while that try tests it, and gone when
    /// the answer is adopted. A real driver is the field's to show.
    #[test]
    fn media_foundation_comes_back_with_no_restart() {
        let _fixture = probe_fixture();
        let (first, _) = probe_scripted(
            &a_first_probe(),
            vec![
                ("base", Some(base_stuck_in("hevc_nvenc:yuv420"))),
                ("base", Some(base_reported())),
            ],
        );
        cells::install_learned(first.learned.clone());
        let in_force = |name: &str| cell_denied(&denied_cells(), name, ChromaFormat::Yuv420);
        assert_eq!(in_force(cells::MF_H264), cfg!(windows));

        let seed = list_for_a_try(&cells::learned());
        assert!(seed.is_empty(), "both were only for want of time: {seed:?}");
        let (got, asked) = probe_scripted(
            &a_later_try(seed.clone()),
            vec![("base", Some(base_reported()))],
        );
        assert!(asked[0].is_empty(), "the child may open them");
        assert_eq!(
            in_force(cells::MF_H264),
            cfg!(windows),
            "and a session still may not, until an answer is adopted"
        );
        assert_eq!(got.short, None);
        assert!(
            weigh(
                &compute_caps(false, false),
                &[],
                &seed,
                &first.short.clone().expect("short"),
                &got
            )
            .adopt
        );
        cells::install_learned(got.learned);
        assert!(!in_force(cells::MF_H264));
    }

    fn with_cells(list: &[(VideoCodec, VideoBackend)]) -> AgentCaps {
        AgentCaps {
            video_cells: list
                .iter()
                .map(|(codec, backend)| {
                    VideoCell::new(*codec, *backend, &[ChromaFormat::Yuv420], true)
                })
                .collect(),
            ..AgentCaps::default()
        }
    }

    fn got_with(caps: Option<AgentCaps>, short: Option<child::Shortfall>) -> child::Probed {
        child::Probed {
            report: caps.map(|caps| child::ProbeReport {
                caps,
                vp9_qsv_idr: None,
                gpu_input: Vec::new(),
            }),
            learned: Vec::new(),
            short,
            unanswered: false,
            stopped: false,
        }
    }

    const NVENC: (VideoCodec, VideoBackend) = (VideoCodec::Hevc, VideoBackend::Nvenc);
    const QSV: (VideoCodec, VideoBackend) = (VideoCodec::H264, VideoBackend::Qsv);
    const AMF: (VideoCodec, VideoBackend) = (VideoCodec::H264, VideoBackend::Amf);

    fn short_by(cells: &[&str]) -> child::Shortfall {
        child::Shortfall {
            base: false,
            cells: cells.iter().map(|c| c.to_string()).collect(),
            forms: false,
        }
    }

    /// The normal end: an answer with no shortfall, adopted, and no more tries.
    #[test]
    fn a_complete_answer_is_adopted_and_ends_the_climb() {
        let verdict = weigh(
            &with_cells(&[QSV]),
            &[],
            &[],
            &short_by(&["hevc_nvenc:yuv420"]),
            &got_with(Some(with_cells(&[QSV, NVENC])), None),
        );
        assert_eq!(
            verdict,
            Verdict {
                adopt: true,
                next: Next::End("it answered"),
            }
        );
    }

    /// A complete answer with the same forms still replaces the one in force: it is
    /// what takes the word and the denies for want of time away.
    #[test]
    fn a_complete_answer_with_the_same_cells_is_adopted() {
        let verdict = weigh(
            &with_cells(&[QSV]),
            &[],
            &[],
            &child::Shortfall::default(),
            &got_with(Some(with_cells(&[QSV])), None),
        );
        assert!(verdict.adopt);
    }

    /// A try cannot take a form from a host. Fewer forms, and more forms that leave
    /// one out, are both dropped, GPU-input forms included.
    #[test]
    fn a_try_never_takes_a_cell() {
        let last = child::Shortfall::default();
        let complete_but_smaller = weigh(
            &with_cells(&[QSV, NVENC]),
            &[],
            &[],
            &last,
            &got_with(Some(with_cells(&[QSV])), None),
        );
        assert_eq!(
            complete_but_smaller,
            Verdict {
                adopt: false,
                next: Next::End("it answered"),
            }
        );

        let more_but_not_the_same = weigh(
            &with_cells(&[NVENC]),
            &[],
            &[],
            &last,
            &got_with(Some(with_cells(&[QSV, AMF])), Some(short_by(&["x:yuv420"]))),
        );
        assert!(!more_but_not_the_same.adopt);

        let lost_a_gpu_form = weigh(
            &with_cells(&[QSV]),
            &["h264_qsv".to_string()],
            &[],
            &last,
            &got_with(Some(with_cells(&[QSV])), None),
        );
        assert!(!lost_a_gpu_form.adopt);
    }

    /// Still short, but with more than is in force: a host gets what it has found
    /// so far, and the climb goes on.
    #[test]
    fn a_short_answer_with_more_cells_is_adopted_and_climbs() {
        let now = short_by(&["av1_nvenc:yuv420"]);
        let verdict = weigh(
            &with_cells(&[QSV]),
            &[],
            &[],
            &short_by(&["hevc_nvenc:yuv420"]),
            &got_with(Some(with_cells(&[QSV, NVENC])), Some(now.clone())),
        );
        assert_eq!(
            verdict,
            Verdict {
                adopt: true,
                next: Next::Climb(now),
            }
        );

        // With nothing new it waits for a better one.
        let same = weigh(
            &with_cells(&[QSV]),
            &[],
            &[],
            &short_by(&["hevc_nvenc:yuv420"]),
            &got_with(Some(with_cells(&[QSV])), Some(short_by(&["other:yuv420"]))),
        );
        assert!(!same.adopt);
        assert!(matches!(same.next, Next::Climb(_)));
    }

    /// The one thing that takes a cell: a new fault in a form the answer in force
    /// advertises. No session can be left to find it. Old news changes nothing.
    #[test]
    fn a_new_fault_in_a_cell_in_force_takes_that_cell() {
        let fault = LearnedDeny {
            cell: "hevc_nvenc:yuv420".to_string(),
            why: "exit code: 0xc0000005".to_string(),
            confirmed: true,
            timed_out: false,
        };
        let got = child::Probed {
            learned: vec![fault.clone()],
            ..got_with(Some(with_cells(&[QSV])), None)
        };
        let last = child::Shortfall::default();
        assert!(weigh(&with_cells(&[QSV, NVENC]), &[], &[], &last, &got).adopt);
        assert!(
            !weigh(
                &with_cells(&[QSV, NVENC]),
                &[],
                std::slice::from_ref(&fault),
                &last,
                &got
            )
            .adopt,
            "already on the list when this try started"
        );
        assert!(weigh(&with_cells(&[QSV]), &[], &[], &last, &got).adopt);
    }

    /// A try that ends short for a reason other than time has nothing more to learn
    /// from time.
    #[test]
    fn a_try_short_for_another_reason_ends_the_climb() {
        let verdict = weigh(
            &with_cells(&[QSV]),
            &[],
            &[],
            &child::Shortfall {
                base: true,
                ..Default::default()
            },
            &got_with(None, None),
        );
        assert_eq!(
            verdict,
            Verdict {
                adopt: false,
                next: Next::End("it left no answer, and not for want of time"),
            }
        );
    }

    /// The signature of a real hang: the same cell, with more time. Another try
    /// could hold the GPU for minutes on a host someone may be using.
    #[test]
    fn the_same_cell_out_of_time_again_ends_the_climb() {
        let cell = "hevc_nvenc:yuv420";
        let verdict = weigh(
            &with_cells(&[QSV]),
            &[],
            &[],
            &short_by(&[cell]),
            &got_with(Some(with_cells(&[QSV])), Some(short_by(&[cell]))),
        );
        assert_eq!(
            verdict.next,
            Next::End("the same cell ran out of time again, with more of it")
        );
        assert!(!verdict.adopt);

        // A blind hang has no cell to repeat, and goes on up the table.
        let blind = child::Shortfall {
            base: true,
            ..Default::default()
        };
        let verdict = weigh(
            &with_cells(&[QSV]),
            &[],
            &[],
            &blind,
            &got_with(None, Some(blind.clone())),
        );
        assert_eq!(verdict.next, Next::Climb(blind));
    }

    /// A session stopped it, or went live before it could go in: nothing it found
    /// counts, whatever it holds.
    #[test]
    fn an_answer_waits_for_a_live_session_and_a_stopped_try_is_lost() {
        let got = child::Probed {
            stopped: true,
            ..got_with(Some(with_cells(&[QSV, NVENC])), None)
        };
        assert_eq!(
            weigh(
                &with_cells(&[QSV]),
                &[],
                &[],
                &child::Shortfall::default(),
                &got
            ),
            Verdict {
                adopt: false,
                next: Next::Again,
            }
        );
    }

    /// A try a session stopped is not counted: the same row, owing only quiet.
    #[test]
    fn a_stopped_try_runs_the_same_row_again() {
        let mut waits = Vec::new();
        let mut rows = Vec::new();
        let mut script = vec![
            Next::Again,
            Next::Again,
            Next::Climb(child::Shortfall::default()),
            Next::End("done"),
        ]
        .into_iter();
        let why = climb(
            child::Shortfall::default(),
            |owed| waits.push(owed),
            |row, _| {
                rows.push(row);
                script.next().expect("a try the case does not expect")
            },
        );
        assert_eq!(why, "done");
        assert_eq!(rows, [0, 0, 0, 1]);
        assert_eq!(
            waits,
            [
                LADDER[0].wait,
                Duration::ZERO,
                Duration::ZERO,
                LADDER[1].wait
            ]
        );
    }

    /// Five tries and no more, each told what the last was short by, each with at
    /// least as long as the one before.
    #[test]
    fn the_climb_ends_at_the_end_of_the_table() {
        let mut seen = Vec::new();
        let short = |n: usize| short_by(&[format!("try{n}:yuv420").as_str()]);
        let why = climb(
            short(0),
            |_| {},
            |row, last| {
                seen.push(last.clone());
                Next::Climb(short(row + 1))
            },
        );
        assert_eq!(why, "the end of the table");
        assert_eq!(seen, (0..LADDER.len()).map(short).collect::<Vec<_>>());

        assert_eq!(LADDER.len(), 5);
        assert!(LADDER[0].bounds.child > child::Bounds::FIRST.child);
        assert!(LADDER[0].bounds.kill_wait > child::Bounds::FIRST.kill_wait);
        assert!(LADDER[0].bounds.base_budget > child::Bounds::FIRST.base_budget);
        for pair in LADDER.windows(2) {
            assert!(pair[1].wait > pair[0].wait);
            assert!(pair[1].bounds.child >= pair[0].bounds.child);
            assert!(pair[1].bounds.kill_wait >= pair[0].bounds.kill_wait);
            assert!(pair[1].bounds.base_budget >= pair[0].bounds.base_budget);
        }
    }

    /// The kill switch, read at each try: nothing starts, and no child runs.
    #[test]
    fn the_kill_switch_ends_the_climb_before_any_child() {
        use tunnel_core::env::test_env::set;
        let _fixture = probe_fixture();
        assert!(retry_enabled(), "on by default");
        // SAFETY: serialised by DENY_ENV_LOCK; the fixture restores it.
        unsafe { set("CAPS_PROBE_RETRY", "0") };
        assert!(!retry_enabled());
        assert_eq!(
            one_try(0, &child::Shortfall::default(), &|| false),
            Next::End("ROOMLERD_CAPS_PROBE_RETRY is off")
        );
    }

    /// A process arms once, however many organizations it serves.
    #[test]
    fn the_first_call_takes_the_shortfall() {
        use tunnel_core::env::test_env::set;
        let _fixture = probe_fixture();
        // SAFETY: serialised by DENY_ENV_LOCK; the fixture restores it. Off, so
        // that nothing starts a thread.
        unsafe { set("CAPS_PROBE_RETRY", "0") };
        *FIRST_SHORT.lock().unwrap() = Some(child::Shortfall::default());
        arm_retry(|| false);
        assert_eq!(*FIRST_SHORT.lock().unwrap(), None);
    }

    /// A try's answer takes the cache's gate unchanged: short of time, or with a
    /// form unanswered, or with no hardware, it is not frozen.
    #[test]
    fn a_trys_answer_passes_the_caches_gate_unchanged() {
        let _fixture = probe_fixture();
        let (got, _) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(answer_444(&["hevc_nvenc"]))),
            ],
        );
        let report = got.report.expect("a report");
        assert_eq!(
            why_not_cached(&report.caps, &got.learned, got.unanswered),
            None
        );

        let (got, _) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![
                ("base", Some(base_hung_in("av1_nvenc:yuv420"))),
                ("base", Some(report_of(nvenc_caps()))),
                ("444", Some(answer_444(&[]))),
            ],
        );
        let report = got.report.expect("a report");
        assert!(why_not_cached(&report.caps, &got.learned, got.unanswered).is_some());

        let (got, _) = probe_scripted(
            &a_later_try(Vec::new()),
            vec![("base", Some(base_reported()))],
        );
        let report = got.report.expect("a report");
        assert!(why_not_cached(&report.caps, &got.learned, got.unanswered).is_some());
    }

    /// A host short of time with no cell to name says so, under the key old readers
    /// already skip, and the word goes when an answer comes.
    #[test]
    fn a_host_short_of_time_says_why() {
        use roomler_ai_remote_control::models::OsKind;
        let short = child::Shortfall {
            base: true,
            ..Default::default()
        };
        let first =
            assigned_by_the_daemon(p0c_caps(), &p0c_facts(OsKind::Windows), &[], Some(&short));
        assert_eq!(first.unavailable[LEARNED_WORD].code, SHORT_CODE);

        // A named cell says more than a short answer can.
        let hung = LearnedDeny {
            cell: "hevc_nvenc:yuv420".to_string(),
            why: "hung".to_string(),
            confirmed: false,
            timed_out: true,
        };
        let named = assigned_by_the_daemon(
            p0c_caps(),
            &p0c_facts(OsKind::Windows),
            std::slice::from_ref(&hung),
            Some(&short),
        );
        assert_eq!(named.unavailable[LEARNED_WORD].code, LEARNED_CODE);

        let gained = adopted_answer(first.clone(), nvenc_caps(), &[], None);
        assert!(
            !gained.unavailable.contains_key(LEARNED_WORD),
            "{:?}",
            gained.unavailable
        );
        let still = adopted_answer(first, nvenc_caps(), &[], Some(&short));
        assert_eq!(still.unavailable[LEARNED_WORD].code, SHORT_CODE);
    }

    /// Adopting moves what a probe answers and nothing the daemon says for itself.
    #[test]
    fn adopting_moves_only_what_a_probe_answers() {
        use roomler_ai_remote_control::models::OsKind;
        let mut in_force =
            assigned_by_the_daemon(p0c_caps(), &p0c_facts(OsKind::Windows), &[], None);
        in_force.files = vec!["the daemon's own".to_string()];
        in_force.probe_cached = true;
        let mut answer = nvenc_caps();
        answer.files = vec!["the child's built-in default".to_string()];
        answer.codecs = vec!["h265".to_string()];
        answer.hw_encoders = vec!["ffmpeg-hevc_nvenc".to_string()];
        answer.transports = vec!["data-channel-hevc".to_string()];
        answer.hevc_chroma = vec!["yuv420".to_string()];
        answer.vp9_chroma = "yuv444".to_string();
        answer.probe_ms = Some(70_000);
        let want_cells = answer.video_cells.clone();

        let next = adopted_answer(in_force.clone(), answer, &[], None);
        assert_eq!(next.codecs, ["h265"]);
        assert_eq!(next.hw_encoders, ["ffmpeg-hevc_nvenc"]);
        assert_eq!(next.transports, ["data-channel-hevc"]);
        assert_eq!(next.hevc_chroma, ["yuv420"]);
        assert_eq!(next.vp9_chroma, "yuv444");
        assert_eq!(next.video_cells, want_cells);
        assert_eq!(next.probe_ms, Some(70_000));
        assert!(!next.probe_cached, "it is not the cache's answer");
        assert_eq!(next.files, in_force.files, "config-derived words stay");
        assert_eq!(next.rpc, in_force.rpc);
        assert_eq!(next.apps, in_force.apps);
        assert!(next.unavailable.contains_key("sas"), "and honest words");
    }
}
