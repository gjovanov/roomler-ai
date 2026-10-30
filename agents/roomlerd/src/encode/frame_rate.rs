// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-119 P1 — the frame rate a viewer asks for, and the answer it gets.
//!
//! A viewer on a high-refresh screen asks with `rc:fps {fps}` on the control
//! channel, and only of an agent that advertises [`CAP`] in `AgentCaps.video`.
//! The control handler stores the ask (`ViewerFeedback::store_fps_ask`). The
//! pump answers in `rc:video-info`, the message that already describes the
//! stream: `fps` is what the stream runs at, and when the viewer asked for
//! more, `fps_asked` and `fps_reason` say what was asked and why it was not
//! granted. The pump knows its own cap and transport, so it is the one place
//! the answer can be correct, including before the encoder exists.
//!
//! P1 grants nothing above the session's current cap. P2 builds the grant: a
//! direct path, a rate class a session has proven, and a pipe that covers the
//! ceiling (`docs/fr/FR-119-high-refresh-streaming.md` §3.2).

/// The capability verb in `AgentCaps.video`.
///
/// ⚠️ A wire spelling, matched by equality: renaming it makes every agent look
/// like it lacks the feature.
pub const CAP: &str = "fps-high";

/// P2b-17b — this agent acts on `rc:fps {fps, retry: true}`: it forgets the
/// session's verdict for the class asked for and decides again, so a class
/// refused as unproven is granted as a trial. The viewer offers "try again"
/// only to an agent advertising it, because an older one ignores the field.
///
/// ⚠️ A wire spelling, matched by equality, like [`CAP`].
pub const RETRY_CAP: &str = "fps-retry";

/// Every frame-rate verb this agent advertises when `fps-high` is on.
pub const CAPS: [&str; 2] = [CAP, RETRY_CAP];

/// The rates a viewer may ask for.
pub const CHOICES: [u32; 3] = [60, 120, 144];

/// The ask in an `rc:fps` message, when it is one of [`CHOICES`]. Anything else
/// is dropped, the way `rc:resolution` drops a mode it does not know.
pub fn parse_ask(msg: &serde_json::Value) -> Option<u32> {
    let fps = u32::try_from(msg.get("fps")?.as_u64()?).ok()?;
    CHOICES.contains(&fps).then_some(fps)
}

/// P2b-17b — whether an `rc:fps` message asks to trial its rate again. Only a
/// literal `true` counts: anything else is an ordinary ask.
pub fn parse_retry(msg: &serde_json::Value) -> bool {
    msg.get("retry").and_then(serde_json::Value::as_bool) == Some(true)
}

/// Whether an agent's video verbs name any frame-rate verb ([`CAPS`]).
#[cfg(test)]
pub(crate) fn any_verb_in(features: &[String]) -> bool {
    features.iter().any(|f| CAPS.contains(&f.as_str()))
}

/// Why the stream runs below what the viewer asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A relayed (constrained) path. Above its cap waits for FR-17 stage C,
    /// FR-63 and FR-70 M3.
    Constrained,
    /// The host has not proven it can sustain the rate, or a trial on it
    /// failed. In P1 no host has.
    Unproven,
    /// FR-119 P2b — the captured display refreshes slower than the ask: a
    /// screen cannot produce frames faster than it refreshes.
    Refresh,
    /// FR-119 P2b — `fps_deny` names this encoder at this rate.
    Denied,
    /// FR-119 P2b — what the session believes the pipe carries does not cover
    /// the ceiling the higher rate needs.
    Pipe,
}

impl Reason {
    /// ⚠️ A wire spelling, read by the viewer.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Constrained => "constrained",
            Self::Unproven => "unproven",
            Self::Refresh => "refresh",
            Self::Denied => "denied",
            Self::Pipe => "pipe",
        }
    }
}

// ── FR-119 P2b — the grant, the rate class, the trial ───────────────────

/// What a rate class depends on: the things that bound how many frames a host
/// can produce and encode. A class proven on one is no evidence for another.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClassKey {
    /// The capture backend serving the session, as it names itself
    /// (`ScreenCapture::backend`: `wgc`, `scrap`, `drm`, `mutter`, `portal`,
    /// `system-context`, …).
    pub backend: String,
    /// The captured display's refresh rate, in Hz; 0 when unknown.
    pub refresh_hz: u16,
    /// The FFmpeg encoder (`hevc_videotoolbox`, `av1_nvenc`, …).
    pub encoder: String,
    /// The encode geometry's long edge, bucketed ([`long_edge_bucket`]). A
    /// class proven at 1080p says nothing about 4K.
    pub long_edge: u32,
    /// The rate.
    pub fps: u32,
}

impl ClassKey {
    /// P2b-18 — this class at another rate, for the step down.
    pub fn at(&self, fps: u32) -> Self {
        Self {
            fps,
            ..self.clone()
        }
    }

    /// The same backend, screen, encoder and geometry, whatever the rate.
    /// ⚠️ Destructured so a new field cannot be left out of the comparison.
    fn same_but_fps(&self, other: &Self) -> bool {
        let Self {
            backend,
            refresh_hz,
            encoder,
            long_edge,
            fps: _,
        } = self;
        *backend == other.backend
            && *refresh_hz == other.refresh_hz
            && *encoder == other.encoder
            && *long_edge == other.long_edge
    }
}

/// The encode geometry's long edge, rounded up to the next of 1280, 1920,
/// 2560 or 3840; anything larger is one bucket.
pub fn long_edge_bucket(width: u32, height: u32) -> u32 {
    let edge = width.max(height);
    [1280, 1920, 2560, 3840]
        .into_iter()
        .find(|&b| edge <= b)
        .unwrap_or(u32::MAX)
}

/// What a trial concluded about a class.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A trial sustained the rate: grant it without one next time.
    Proven,
    /// A trial fell short: do not trial again for an hour (P2b-17a,
    /// `fps_class_cache::FAILED_MAX_AGE_SECS`), or until the build, the
    /// hardware or the knobs change (the cache's key).
    Failed,
}

/// What the grant decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantInputs {
    /// The viewers' ask, merged by `min` (a viewer on a 60 Hz screen never
    /// receives a heavier stream than it asked for).
    pub asked: Option<u32>,
    /// Today's cap for this session (60 direct, 30 constrained).
    pub cap: u32,
    pub constrained: bool,
    /// The captured display's refresh rate, when the agent could read it.
    pub refresh_hz: Option<u16>,
    /// P2b-18 — what the grant reads about each rate in [`CHOICES`], in that
    /// order, so a refused ask can step down one rate at a time
    /// ([`rate_facts`] builds it).
    pub rates: [RateFacts; CHOICES.len()],
}

/// FR-119 P2b-18 — what the grant reads about one rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateFacts {
    /// `fps_deny` names this encoder at this rate.
    pub denied: bool,
    /// The class's cached verdict at this rate; `None` = never tried.
    /// [`RateState::decide`] fills it.
    pub verdict: Option<Verdict>,
    /// The session's pipe belief covers the ceiling at this rate.
    pub pipe_covers: bool,
}

impl RateFacts {
    /// Nothing against the rate: not denied, never tried, and no push-back.
    pub const OPEN: Self = Self {
        denied: false,
        verdict: None,
        pipe_covers: true,
    };
}

/// [`GrantInputs::rates`] for a session. Each rate above `cap` and up to the
/// ask gets its own deny entry and pipe check; the grant never reads any
/// other, so those stay [`RateFacts::OPEN`].
pub fn rate_facts(
    asked: Option<u32>,
    cap: u32,
    denied: impl Fn(u32) -> bool,
    pipe_covers: impl Fn(u32) -> bool,
) -> [RateFacts; CHOICES.len()] {
    CHOICES.map(|fps| {
        if asked.is_some_and(|a| fps > cap && fps <= a) {
            RateFacts {
                denied: denied(fps),
                verdict: None,
                pipe_covers: pipe_covers(fps),
            }
        } else {
            RateFacts::OPEN
        }
    })
}

impl GrantInputs {
    /// The facts for `fps`; `None` for a rate outside [`CHOICES`].
    fn facts(&self, fps: u32) -> Option<&RateFacts> {
        CHOICES
            .iter()
            .position(|&c| c == fps)
            .map(|i| &self.rates[i])
    }

    /// Why `fps` cannot be granted, checked in order of what a viewer can see
    /// and do something about: the transport, the screen, the owner's denial,
    /// the host's record, the pipe. `None` when nothing stands in its way.
    fn refusal_at(&self, fps: u32) -> Option<Reason> {
        if self.constrained {
            return Some(Reason::Constrained);
        }
        if self
            .refresh_hz
            .is_some_and(|hz| hz > 0 && u32::from(hz) < fps)
        {
            return Some(Reason::Refresh);
        }
        // A rate this agent does not offer has no record to show for it.
        let Some(f) = self.facts(fps) else {
            return Some(Reason::Unproven);
        };
        if f.denied {
            Some(Reason::Denied)
        } else if f.verdict == Some(Verdict::Failed) {
            Some(Reason::Unproven)
        } else if !f.pipe_covers {
            Some(Reason::Pipe)
        } else {
            None
        }
    }
}

/// [`GrantInputs::pipe_covers`]: nothing this session measured says the path
/// carries less than `ceiling_bps`, the ceiling at the asked rate.
///
/// `capacity_bps` is the FR-79 belief's pushed-back estimate (`pipe.rs`
/// `capacity_bps`), and `None`, no push-back yet, is no claim to contradict.
/// ⚠️ Never the belief's floor: a floor is what the path has carried so far,
/// which at 60 fps is the content's weight at 60, always under the ceiling at
/// 120. Read as a limit, it would refuse every healthy session.
pub fn pipe_covers(capacity_bps: Option<u32>, ceiling_bps: u32) -> bool {
    capacity_bps.is_none_or(|c| c >= ceiling_bps)
}

/// The grant: the answer for `rc:video-info`, and whether the higher rate is
/// granted as a TRIAL (an unproven class) rather than a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    pub answer: Answer,
    pub trial: bool,
}

/// Above the cap only when every condition holds (spec §3.2), as
/// [`GrantInputs::refusal_at`] checks them. A class never tried is granted as
/// a trial.
///
/// FR-119 P2b-18 — a refused ask steps down one rate at a time: the highest
/// rate in [`CHOICES`] below the ask and above the cap that nothing refuses
/// is granted instead, and the viewer is still told why the ask itself was
/// not. A 144 the host cannot sustain runs at a 120 it can, before anything
/// falls back to the cap. The operator's rule (2026-10-27): "from 144 go down
/// to 120, if still unstable, go to 60". A relay refuses every rate above
/// its cap, so it never steps down.
pub fn grant(i: &GrantInputs) -> Grant {
    let Some(asked) = i.asked.filter(|&a| a > i.cap) else {
        return Grant {
            answer: answer(i.asked, i.cap, i.constrained),
            trial: false,
        };
    };
    let never_tried = |fps| i.facts(fps).is_some_and(|f| f.verdict.is_none());
    let Some(why) = i.refusal_at(asked) else {
        return Grant {
            answer: Answer {
                fps: asked,
                refused: None,
            },
            trial: never_tried(asked),
        };
    };
    let below = CHOICES
        .iter()
        .rev()
        .copied()
        .find(|&fps| fps > i.cap && fps < asked && i.refusal_at(fps).is_none());
    Grant {
        answer: Answer {
            fps: below.unwrap_or(i.cap),
            refused: Some((asked, why)),
        },
        trial: below.is_some_and(never_tried),
    }
}

/// How much MOVING content a trial needs before it may conclude.
pub const TRIAL_ACTIVE: std::time::Duration = std::time::Duration::from_secs(10);
/// A trial passes when the active stretches ran at this share of the rate…
pub const TRIAL_MIN_RATE_PCT: u64 = 90;
/// …and their p99 inter-frame interval stayed under this many periods.
pub const TRIAL_MAX_P99_PERIODS: u64 = 2;
/// Two frames further apart than this many periods are not one stretch of
/// motion: the content paused.
pub const TRIAL_STRETCH_GAP_PERIODS: u64 = 3;
/// A trial fails only when the encoder's p90 time per frame reached this share
/// of a period: then the host itself cannot keep up, whatever the content did.
pub const TRIAL_MAX_ENCODE_PCT: u64 = 90;
/// …and only when the encoder spaced the frames: their median interval at most
/// this share of the encoder's median time per frame (FR-119 P2b-14). Frames
/// further apart than that waited on the content, and an encoder timed at such
/// a light load is no measure of it at full load.
pub const TRIAL_HOST_BOUND_INTERVAL_PCT: u64 = 150;

/// FR-119 P2b — the trial: does this host sustain `fps` while the content
/// moves?
///
/// ⚠️ Capture is event-driven, so an idle desktop produces few frames (the
/// fleet's median window delivers 16–17 fps with 60 targeted). A trial that
/// judged every second would fail every capable host that sat still. So it
/// judges only **active stretches**: runs of frames at most
/// [`TRIAL_STRETCH_GAP_PERIODS`] periods apart. A pause, typing, or a 30 fps
/// video (33 ms apart, more than three 120 fps periods) is not evidence either
/// way.
///
/// ⚠️ **Proven** needs the frames themselves: [`TRIAL_MIN_RATE_PCT`] of the
/// rate over [`TRIAL_ACTIVE`] of motion. **Failed** needs the host to be the
/// limit: the encoder's p90 time per frame at [`TRIAL_MAX_ENCODE_PCT`] of a
/// period (11 ms frames against 8.3 ms). Content that changes slower than the
/// rate (a 60 fps video; a browser page held at 60 on a 120 Hz panel) falls
/// short of the first while the encoder keeps up, and that is no verdict at
/// all: the window starts over, and a later burst of fast content may still
/// prove the class. Counting it as a failure would cache a capable host as
/// unable for a week.
///
/// ⚠️ **An encoder timed on slow content is not timed at all** (P2b-14). A
/// laptop GPU clocks itself down under a light load, so the same encoder is
/// slower per frame on 60 fps content than on 120. Field 2026-10-25, neo16:
/// `av1_nvenc` at 2560×1600 benched 4.4 ms p90 flat out, but under a viewer
/// whose drags arrived at 59 fps it drifted from 4.4 to 8.7 ms a frame (the
/// GPU idles in P8), reached a 10.5 ms p90, and failed the 120 fps class for
/// a week. So **Failed** also needs the encoder to have spaced the frames:
/// their median interval within [`TRIAL_HOST_BOUND_INTERVAL_PCT`] of its
/// median time (11 ms frames 11–13 ms apart). Frames 17 ms apart behind a
/// 9 ms encoder waited on the content, and conclude nothing.
#[derive(Debug, Clone)]
pub struct TrialMeter {
    period_us: u64,
    last_us: Option<u64>,
    active_us: u64,
    intervals_us: Vec<u64>,
    /// The encoder's time for each counted frame.
    encode_us: Vec<u64>,
}

impl TrialMeter {
    pub fn new(fps: u32) -> Self {
        Self {
            period_us: 1_000_000 / u64::from(fps.max(1)),
            last_us: None,
            active_us: 0,
            intervals_us: Vec::new(),
            encode_us: Vec::new(),
        }
    }

    /// One frame left the encoder, captured at `at_us`, after `encode_us` in
    /// it. Returns the verdict once [`TRIAL_ACTIVE`] of moving content has
    /// been seen and it concludes one, and `None` until then.
    pub fn frame(&mut self, at_us: u64, encode_us: u64) -> Option<Verdict> {
        let previous = self.last_us.replace(at_us);
        let gap = at_us.saturating_sub(previous?);
        if gap == 0 || gap > self.period_us * TRIAL_STRETCH_GAP_PERIODS {
            return None; // the first frame, a duplicate, or a pause
        }
        self.intervals_us.push(gap);
        self.encode_us.push(encode_us);
        self.active_us += gap;
        if self.active_us < TRIAL_ACTIVE.as_micros() as u64 {
            return None;
        }
        let verdict = self.verdict();
        if verdict.is_none() {
            // The content was slower than the rate and the encoder kept up:
            // judge a fresh window.
            self.active_us = 0;
            self.intervals_us.clear();
            self.encode_us.clear();
        }
        verdict
    }

    /// The pump dropped a frame for the viewer or the link (the viewer-rate
    /// divisor, the byte gate): the stretch ends here. The gap across a drop
    /// says how fast the viewer decodes or the path drains, not how fast this
    /// host captures and encodes, and the verdict is cached as a fact about
    /// the host. Counted, one dropped frame in fifty puts a two-period gap at
    /// the p99 and fails a host that kept up.
    pub fn pause(&mut self) {
        self.last_us = None;
    }

    /// FR-119 P2b-3 — what the current window measured. Read when a verdict
    /// is returned, it is what the verdict was decided on.
    pub fn summary(&self) -> TrialSummary {
        TrialSummary {
            frames: self.intervals_us.len(),
            active_ms: self.active_us / 1_000,
            fps_x10: self.intervals_us.len() as u64 * 10_000_000 / self.active_us.max(1),
            p99_interval_us: percentile(&self.intervals_us, 99),
            p90_encode_us: percentile(&self.encode_us, 90),
        }
    }

    fn verdict(&self) -> Option<Verdict> {
        // Achieved rate over the active time, against the target, in percent.
        let achieved_pct =
            self.intervals_us.len() as u64 * self.period_us * 100 / self.active_us.max(1);
        if achieved_pct >= TRIAL_MIN_RATE_PCT
            && percentile(&self.intervals_us, 99) < self.period_us * TRIAL_MAX_P99_PERIODS
        {
            Some(Verdict::Proven)
        } else if percentile(&self.encode_us, 90) * 100 >= self.period_us * TRIAL_MAX_ENCODE_PCT
            && percentile(&self.intervals_us, 50) * 100
                <= percentile(&self.encode_us, 50) * TRIAL_HOST_BOUND_INTERVAL_PCT
        {
            Some(Verdict::Failed)
        } else {
            None
        }
    }
}

/// The `q`th percentile of `v` (nearest rank, rounded down); 0 for nothing.
fn percentile(v: &[u64], q: usize) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    s[(s.len() * q / 100).min(s.len() - 1)]
}

/// FR-119 P2b-3 — what a concluded trial measured, for its log line: a field
/// run reads the numbers the verdict was decided on, not the verdict alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrialSummary {
    /// The frames counted, and the motion they covered.
    pub frames: usize,
    pub active_ms: u64,
    /// The achieved rate, in tenths of a frame per second.
    pub fps_x10: u64,
    /// The p99 interval between counted frames.
    pub p99_interval_us: u64,
    /// The encoder's p90 time per counted frame.
    pub p90_encode_us: u64,
}

/// FR-119 P2b-2 — one session's frame rate: what the grant decided, what the
/// stream runs at, what this session knows about each rate class, and the
/// trial that may be judging one.
///
/// It is the pump's whole interface to the decision. The pump tells it what
/// moved ([`Self::decide`]), what the stream now runs at
/// ([`Self::running_at`]), each frame that left the encoder ([`Self::frame`])
/// and each one it dropped for the viewer or the link ([`Self::pause`]). It
/// asks what to run at ([`Self::want`]) and what to tell the viewer
/// ([`Self::video_info_fields`]). Pure, so every path is unit-tested.
#[derive(Debug, Clone)]
pub struct RateState {
    running: u32,
    /// What this session knows: the cache's verdicts at the start, then its
    /// own trials'. The newest for a class is last.
    verdicts: Vec<(ClassKey, Verdict)>,
    /// The last decision and what it was decided on.
    decided: Option<(GrantInputs, ClassKey, Grant)>,
    trial: Option<(ClassKey, TrialMeter)>,
    /// P2b-3 — what the last trial to conclude measured.
    concluded: Option<TrialSummary>,
}

impl RateState {
    pub fn new(running: u32, verdicts: Vec<(ClassKey, Verdict)>) -> Self {
        Self {
            running,
            verdicts,
            decided: None,
            trial: None,
            concluded: None,
        }
    }

    /// The newest verdict for `class` at `fps`.
    fn verdict_at(&self, class: &ClassKey, fps: u32) -> Option<Verdict> {
        self.verdicts
            .iter()
            .rev()
            .find(|(k, _)| k.fps == fps && k.same_but_fps(class))
            .map(|(_, v)| *v)
    }

    /// Decide again, if anything the grant reads has moved. `class` is the
    /// session's at the asked rate. The verdicts are this state's to fill:
    /// each rate's in `inputs.rates` is overwritten with this session's for
    /// the class at that rate (P2b-18: every rate the grant may step down
    /// to). Returns `true` when the decision changed.
    pub fn decide(&mut self, mut inputs: GrantInputs, class: ClassKey) -> bool {
        for (facts, fps) in inputs.rates.iter_mut().zip(CHOICES) {
            facts.verdict = self.verdict_at(&class, fps);
        }
        if let Some((i, c, _)) = &self.decided
            && *i == inputs
            && *c == class
        {
            return false;
        }
        let g = grant(&inputs);
        let changed = self.decided.as_ref().is_none_or(|(_, _, old)| *old != g);
        self.decided = Some((inputs, class, g));
        self.sync_trial();
        changed
    }

    /// P2b-17b — [`Self::decide`], after forgetting this session's failed
    /// verdicts for the class asked for when the viewer asked to try it again
    /// (`rc:fps {retry: true}`). A class refused as unproven is then granted
    /// as a trial, and the trial's verdict replaces the cached one, as any
    /// trial's does. Asking again for a class with no failed verdict changes
    /// nothing.
    ///
    /// P2b-18 — it forgets the failed verdicts of every rate from the ask
    /// down to the cap, so the step down is walked again from the top:
    /// "try 144 again" means 144, then 120 if 144 falls short, then the cap.
    /// Kept, a 120 that failed ten minutes earlier would send a 144 that
    /// falls short straight to the cap. A proven rate stays proven.
    pub fn decide_with_retry(&mut self, inputs: GrantInputs, class: ClassKey, retry: bool) -> bool {
        if retry {
            let top = inputs.asked.unwrap_or(inputs.cap);
            self.verdicts.retain(|(k, v)| {
                !(*v == Verdict::Failed
                    && k.fps > inputs.cap
                    && k.fps <= top
                    && k.same_but_fps(&class))
            });
        }
        self.decide(inputs, class)
    }

    /// The stream now runs at `fps`: a swap was adopted, an encoder opened at
    /// it, or the capture's pace moved down ahead of a swap.
    pub fn running_at(&mut self, fps: u32) {
        self.running = fps;
        self.sync_trial();
    }

    pub fn running(&self) -> u32 {
        self.running
    }

    /// The rate the grant wants the stream at; until there is a decision,
    /// what it runs at.
    pub fn want(&self) -> u32 {
        self.decided
            .as_ref()
            .map_or(self.running, |(_, _, g)| g.answer.fps)
    }

    /// A trial runs exactly while the stream runs at a rate granted as one,
    /// and a new class (other dims, another encoder) starts it over. It
    /// judges the class at the granted rate, which a step down (P2b-18)
    /// makes lower than the ask.
    fn sync_trial(&mut self) {
        let Some((_, class, g)) = &self.decided else {
            self.trial = None;
            return;
        };
        if !(g.trial && self.running == g.answer.fps) {
            self.trial = None;
        } else if self
            .trial
            .as_ref()
            .is_none_or(|(k, _)| k.fps != g.answer.fps || !k.same_but_fps(class))
        {
            self.trial = Some((class.at(g.answer.fps), TrialMeter::new(self.running)));
        }
    }

    /// A frame left the encoder, captured at `at_us`, after `encode_us` in it.
    /// When that concludes the trial, the verdict is this session's from now
    /// on (the decision is taken again at once, so a failed class stops being
    /// wanted), and the class and verdict are returned for the pump to persist.
    /// What the trial measured stays readable ([`Self::concluded`]).
    pub fn frame(&mut self, at_us: u64, encode_us: u64) -> Option<(ClassKey, Verdict)> {
        let verdict = self.trial.as_mut()?.1.frame(at_us, encode_us)?;
        let (class, meter) = self.trial.take()?;
        self.concluded = Some(meter.summary());
        self.verdicts.push((class.clone(), verdict));
        if let Some((inputs, decided_class, _)) = self.decided.clone() {
            self.decided = None;
            self.decide(inputs, decided_class);
        }
        Some((class, verdict))
    }

    /// The replacement at a rate above the cap failed to open. A class this
    /// host cannot even open at is no class to run a trial on, so it is failed
    /// for the rest of the session and decided again at once, which steps
    /// down to the next rate (P2b-18). It is not returned for the cache: an
    /// open can fail for a reason that is not the rate (two encoder sessions
    /// at once on a card at its session limit), and the next session tries
    /// again. `false` when nothing above the cap was granted, so there is
    /// nothing to fail.
    pub fn open_failed(&mut self) -> bool {
        let Some((inputs, class, g)) = self.decided.clone() else {
            return false;
        };
        if g.answer.fps <= inputs.cap {
            return false;
        }
        // The rate that did not open is the granted one: below the ask after
        // a step down.
        self.verdicts
            .push((class.at(g.answer.fps), Verdict::Failed));
        self.decided = None;
        self.decide(inputs, class);
        true
    }

    /// The pump dropped a captured frame for the viewer or the link
    /// ([`TrialMeter::pause`]).
    pub fn pause(&mut self) {
        if let Some((_, meter)) = self.trial.as_mut() {
            meter.pause();
        }
    }

    pub fn trial_running(&self) -> bool {
        self.trial.is_some()
    }

    /// P2b-3 — what the last trial to conclude measured, for its log line.
    pub fn concluded(&self) -> Option<TrialSummary> {
        self.concluded
    }

    /// What the stream runs at and, once it runs below an ask the grant
    /// refused, why. While a swap down is still opening, the stream still
    /// runs at the asked rate, and saying "refused" then would contradict
    /// the rate beside it.
    pub fn answer(&self) -> Answer {
        let refused = self
            .decided
            .as_ref()
            .and_then(|(_, _, g)| g.answer.refused)
            .filter(|&(asked, _)| self.running < asked);
        Answer {
            fps: self.running,
            refused,
        }
    }

    /// FR-119 P2b-12 — the decision's own refusal: what was asked and why it
    /// was not granted, whatever the stream still runs at. [`Self::answer`]
    /// holds a refusal back until the stream is down at the granted rate,
    /// which is right for the viewer and wrong for the decision's log line:
    /// that line is written before the swap down, so it read `refused=None`
    /// beside `want=60` and hid the reason a 120 was taken back (field,
    /// 2026-10-24: `pipe`, spec §4.12).
    pub fn decided_refusal(&self) -> Option<(u32, Reason)> {
        self.decided.as_ref().and_then(|(_, _, g)| g.answer.refused)
    }

    /// The `rc:video-info` fields: [`video_info_fields`] for
    /// [`Self::answer`], plus `fps_trial` while a trial runs.
    pub fn video_info_fields(&self) -> String {
        let mut fields = video_info_fields(&self.answer());
        if self.trial.is_some() {
            fields.push_str(r#","fps_trial":true"#);
        }
        fields
    }
}

/// FR-119 P2b — `fps_deny`: `name:fps` pairs, comma-separated, in the
/// `encoder_cells_deny` shape (`hevc_qsv:120,av1_nvenc:144`); `*` stands for
/// every encoder. It only ever removes classes. An entry that does not parse
/// is skipped, never widened into a match.
pub fn parse_fps_deny(raw: &str) -> Vec<(String, u32)> {
    raw.split(',')
        .filter_map(|entry| {
            let (name, fps) = entry.trim().split_once(':')?;
            let fps = fps.trim().parse::<u32>().ok()?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_string(), fps))
        })
        .collect()
}

/// Does the deny list name this encoder (or `*`) at this rate?
pub fn fps_denied(list: &[(String, u32)], encoder: &str, fps: u32) -> bool {
    list.iter()
        .any(|(name, f)| *f == fps && (name == "*" || name == encoder))
}

/// `ROOMLERD_FPS_DENY`, read once per pump (FR-70: no knob read in the frame
/// path).
pub fn fps_deny_from_env() -> Vec<(String, u32)> {
    tunnel_core::env::node_env("FPS_DENY")
        .map(|raw| parse_fps_deny(&raw))
        .unwrap_or_default()
}

/// What `rc:video-info` says about the frame rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    /// The rate the stream runs at: the pump's target.
    pub fps: u32,
    /// The viewer's ask and why it was not granted, when it asked for more than
    /// the stream runs at. `None` when nothing needs explaining.
    pub refused: Option<(u32, Reason)>,
}

/// P1's answer: the stream stays at the pump's `cap`. An ask at or below it needs
/// no explanation. An ask above it is named, with the reason.
pub fn answer(asked: Option<u32>, cap: u32, constrained: bool) -> Answer {
    let refused = asked.filter(|&a| a > cap).map(|a| {
        let why = if constrained {
            Reason::Constrained
        } else {
            Reason::Unproven
        };
        (a, why)
    });
    Answer { fps: cap, refused }
}

/// The `rc:video-info` fields for an answer, with a leading comma, so the
/// payload builder appends them as it does its other optional fields.
pub fn video_info_fields(a: &Answer) -> String {
    match a.refused {
        Some((asked, why)) => format!(
            r#","fps":{},"fps_asked":{asked},"fps_reason":"{}""#,
            a.fps,
            why.wire()
        ),
        None => format!(r#","fps":{}"#, a.fps),
    }
}

/// FR-119 P2b-3 — the captured display's refresh rate for `rc:video-info`
/// (`refresh_hz`), with a leading comma; nothing when it is unknown. The
/// viewer's picker greys the rates above it.
pub fn refresh_field(hz: Option<u16>) -> String {
    hz.map(|hz| format!(r#","refresh_hz":{hz}"#))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── FR-119 P2b ──────────────────────────────────────────────────────

    #[test]
    fn the_long_edge_rounds_up_to_its_bucket() {
        assert_eq!(long_edge_bucket(1280, 720), 1280);
        assert_eq!(long_edge_bucket(1920, 1080), 1920);
        assert_eq!(
            long_edge_bucket(1200, 1920),
            1920,
            "portrait: the long edge"
        );
        assert_eq!(long_edge_bucket(2560, 1600), 2560);
        assert_eq!(long_edge_bucket(3024, 1964), 3840);
        assert_eq!(long_edge_bucket(5120, 2880), u32::MAX);
    }

    /// `i` with the facts at `fps` changed by `f`.
    fn at(mut i: GrantInputs, fps: u32, f: impl FnOnce(&mut RateFacts)) -> GrantInputs {
        let n = CHOICES
            .iter()
            .position(|&c| c == fps)
            .expect("an offered rate");
        f(&mut i.rates[n]);
        i
    }

    fn inputs() -> GrantInputs {
        at(
            GrantInputs {
                asked: Some(120),
                cap: 60,
                constrained: false,
                refresh_hz: Some(120),
                rates: [RateFacts::OPEN; CHOICES.len()],
            },
            120,
            |f| f.verdict = Some(Verdict::Proven),
        )
    }

    /// Every condition holding on a proven class grants the ask, no trial.
    #[test]
    fn a_proven_class_on_a_direct_path_is_granted() {
        let g = grant(&inputs());
        assert_eq!(
            g.answer,
            Answer {
                fps: 120,
                refused: None
            }
        );
        assert!(!g.trial);
    }

    /// A class never tried is granted as a trial.
    #[test]
    fn an_unproven_class_is_granted_as_a_trial() {
        let g = grant(&at(inputs(), 120, |f| f.verdict = None));
        assert_eq!(g.answer.fps, 120);
        assert!(g.trial);
    }

    /// Each failing condition refuses with its own reason, in the order a
    /// viewer can do something about it.
    #[test]
    fn each_refusal_names_its_reason_in_order() {
        let refused = |i: GrantInputs| grant(&i).answer.refused.map(|(_, why)| why);
        assert_eq!(
            refused(GrantInputs {
                constrained: true,
                refresh_hz: Some(60),
                ..inputs()
            }),
            Some(Reason::Constrained),
            "the transport first, even with a slow screen"
        );
        assert_eq!(
            refused(at(
                GrantInputs {
                    refresh_hz: Some(60),
                    ..inputs()
                },
                120,
                |f| f.denied = true
            )),
            Some(Reason::Refresh)
        );
        assert_eq!(
            refused(at(inputs(), 120, |f| {
                f.denied = true;
                f.verdict = Some(Verdict::Failed);
            })),
            Some(Reason::Denied)
        );
        assert_eq!(
            refused(at(inputs(), 120, |f| {
                f.verdict = Some(Verdict::Failed);
                f.pipe_covers = false;
            })),
            Some(Reason::Unproven)
        );
        assert_eq!(
            refused(at(inputs(), 120, |f| f.pipe_covers = false)),
            Some(Reason::Pipe)
        );
        // With no rate between the ask and the cap, a refused ask runs at
        // the cap, never above it, and is no trial.
        let g = grant(&at(inputs(), 120, |f| f.pipe_covers = false));
        assert_eq!(g.answer.fps, 60);
        assert!(!g.trial);
    }

    /// P2b-18 — a refused 144 runs at 120 when nothing refuses 120: proven,
    /// it runs as a promise; never tried, as a trial. Either way the viewer
    /// is told why 144 was not granted. A failed 120 goes on to the cap.
    #[test]
    fn a_refused_ask_steps_down_one_rate() {
        let failed_144 = at(
            GrantInputs {
                asked: Some(144),
                refresh_hz: Some(240),
                ..inputs()
            },
            144,
            |f| f.verdict = Some(Verdict::Failed),
        );
        let g = grant(&failed_144);
        assert_eq!(
            g.answer,
            Answer {
                fps: 120,
                refused: Some((144, Reason::Unproven)),
            }
        );
        assert!(!g.trial, "120 is proven");
        let g = grant(&at(failed_144, 120, |f| f.verdict = None));
        assert_eq!(g.answer.fps, 120);
        assert!(g.trial, "120 never tried: a trial");
        let g = grant(&at(failed_144, 120, |f| f.verdict = Some(Verdict::Failed)));
        assert_eq!(
            g.answer,
            Answer {
                fps: 60,
                refused: Some((144, Reason::Unproven)),
            }
        );
        assert!(!g.trial);
    }

    /// P2b-18 — every reason that is about the rate steps down: a 120 Hz
    /// screen, a deny entry, a pipe that covers 120's ceiling but not 144's.
    /// A relay is about the path, and refuses every rate above its cap.
    #[test]
    fn each_reason_about_the_rate_steps_down_and_a_relay_does_not() {
        let at_144 = GrantInputs {
            asked: Some(144),
            refresh_hz: Some(240),
            ..inputs()
        };
        let step = |i: GrantInputs| grant(&i).answer;
        assert_eq!(
            step(GrantInputs {
                refresh_hz: Some(120),
                ..at_144
            }),
            Answer {
                fps: 120,
                refused: Some((144, Reason::Refresh)),
            }
        );
        assert_eq!(
            step(at(at_144, 144, |f| f.denied = true)),
            Answer {
                fps: 120,
                refused: Some((144, Reason::Denied)),
            }
        );
        assert_eq!(
            step(at(at_144, 144, |f| f.pipe_covers = false)),
            Answer {
                fps: 120,
                refused: Some((144, Reason::Pipe)),
            }
        );
        let narrow_144 = at(at_144, 144, |f| f.pipe_covers = false);
        assert_eq!(
            step(at(narrow_144, 120, |f| f.pipe_covers = false)),
            Answer {
                fps: 60,
                refused: Some((144, Reason::Pipe)),
            },
            "a pipe too narrow for both: the cap"
        );
        assert_eq!(
            step(GrantInputs {
                constrained: true,
                cap: 30,
                ..at_144
            }),
            Answer {
                fps: 30,
                refused: Some((144, Reason::Constrained)),
            },
            "a relay holds its cap, and 120 and 60 are not offered either"
        );
    }

    /// P2b-18 — the facts the pump builds: only the rates the grant may read
    /// (above the cap, up to the ask) consult the deny list and the pipe.
    #[test]
    fn only_the_rates_the_grant_reads_are_checked() {
        let asked_of = std::cell::RefCell::new(Vec::new());
        let rates = rate_facts(
            Some(120),
            60,
            |fps| {
                asked_of.borrow_mut().push(fps);
                fps == 120
            },
            |_| false,
        );
        assert_eq!(*asked_of.borrow(), vec![120]);
        assert_eq!(
            rates,
            [
                RateFacts::OPEN,
                RateFacts {
                    denied: true,
                    verdict: None,
                    pipe_covers: false,
                },
                RateFacts::OPEN,
            ]
        );
        assert_eq!(
            rate_facts(None, 60, |_| true, |_| false),
            [RateFacts::OPEN; CHOICES.len()],
            "no ask: nothing to check"
        );
    }

    /// An unknown refresh rate (`None`, or 0) decides nothing; a fast enough
    /// screen is no objection.
    #[test]
    fn an_unknown_refresh_rate_decides_nothing() {
        for refresh_hz in [None, Some(0), Some(144)] {
            assert_eq!(
                grant(&GrantInputs {
                    refresh_hz,
                    ..inputs()
                })
                .answer
                .fps,
                120
            );
        }
    }

    /// An ask the cap already covers is P1's answer, whatever else holds.
    #[test]
    fn an_ask_the_cap_covers_is_answered_as_before() {
        for asked in [None, Some(60)] {
            let g = grant(&GrantInputs {
                asked,
                constrained: true,
                ..inputs()
            });
            assert_eq!(
                g.answer,
                Answer {
                    fps: 60,
                    refused: None
                }
            );
            assert!(!g.trial);
        }
    }

    /// Feed a meter frames at the given intervals (µs), each after `encode_us`
    /// in the encoder, until it concludes.
    fn run_trial(
        fps: u32,
        intervals_us: impl IntoIterator<Item = u64>,
        encode_us: u64,
    ) -> Option<Verdict> {
        let mut meter = TrialMeter::new(fps);
        let mut at = 1_000_000u64;
        if let Some(v) = meter.frame(at, encode_us) {
            return Some(v);
        }
        for gap in intervals_us {
            at += gap;
            if let Some(v) = meter.frame(at, encode_us) {
                return Some(v);
            }
        }
        None
    }

    #[test]
    fn a_steady_rate_proves_the_class() {
        assert_eq!(
            run_trial(120, std::iter::repeat_n(8_333, 2_000), 6_000),
            Some(Verdict::Proven)
        );
    }

    /// An encoder that needs 11 ms a frame at 120 fps: frames 11 ms apart,
    /// 76 % of the rate, and the encoder is the reason.
    #[test]
    fn an_encoder_that_cannot_keep_up_fails() {
        assert_eq!(
            run_trial(120, std::iter::repeat_n(11_000, 2_000), 11_000),
            Some(Verdict::Failed)
        );
    }

    /// Content at 60 fps (a 60 fps video, a browser page held at 60 on a 120 Hz
    /// panel) under a 120 fps trial, with an encoder that needs 6 ms: frames
    /// 16.7 ms apart, half the rate, and nothing about the host. No verdict,
    /// however long it runs: each window starts over.
    #[test]
    fn content_slower_than_the_rate_concludes_nothing_while_the_encoder_keeps_up() {
        assert_eq!(
            run_trial(120, std::iter::repeat_n(16_667, 2_000), 6_000),
            None
        );
    }

    /// The same 60 fps content with an encoder that took 9 ms, more than a
    /// 120 fps period. P2b-14: no verdict either. Frames 16.7 ms apart waited
    /// on the content, not on the encoder, and an encoder timed at that load
    /// may be clocked down: the host is not shown unable. (Until P2b-14 this
    /// failed, on the reasoning that the host "could not have kept up with
    /// fast content either".)
    #[test]
    fn a_slow_encoder_on_slow_content_concludes_nothing() {
        assert_eq!(
            run_trial(120, std::iter::repeat_n(16_667, 2_000), 9_000),
            None
        );
    }

    /// FR-119 P2b-14, the field case (2026-10-25, neo16, `av1_nvenc` at
    /// 2560×1600, a viewer whose drags arrived at 59 fps): frames 16.9 ms
    /// apart, the encoder at 10.5 ms a frame because the GPU had clocked down
    /// under that load. It failed the 120 fps class; now it concludes nothing.
    #[test]
    fn a_light_load_with_a_clocked_down_encoder_concludes_nothing() {
        assert_eq!(
            run_trial(120, std::iter::repeat_n(16_900, 2_000), 10_500),
            None
        );
    }

    /// A host that really cannot keep up still fails when the frames carry
    /// some overhead beyond the encode itself (readback, send): 10 ms in the
    /// encoder, frames 12.5 ms apart, so the encoder spaced them.
    #[test]
    fn an_encoder_bound_host_fails_with_per_frame_overhead() {
        assert_eq!(
            run_trial(120, std::iter::repeat_n(12_500, 2_000), 10_000),
            Some(Verdict::Failed)
        );
    }

    /// Slow content first (a window with no verdict), then fast: the fast
    /// burst proves the class.
    #[test]
    fn fast_content_after_slow_still_proves_the_class() {
        let intervals: Vec<u64> = std::iter::repeat_n(16_667u64, 600)
            .chain(std::iter::repeat_n(8_333u64, 2_000))
            .collect();
        assert_eq!(run_trial(120, intervals, 6_000), Some(Verdict::Proven));
    }

    /// Long pauses between bursts of motion count nothing; the bursts alone
    /// conclude the trial.
    #[test]
    fn pauses_between_bursts_are_not_evidence() {
        let burst_then_pause = std::iter::repeat_n(8_333u64, 120).chain(std::iter::once(500_000));
        let intervals: Vec<u64> = std::iter::repeat_n(burst_then_pause, 20)
            .flatten()
            .collect();
        assert_eq!(run_trial(120, intervals, 6_000), Some(Verdict::Proven));
    }

    /// A 30 fps video (33 ms apart) at a 120 fps target is no evidence either
    /// way: the trial never concludes on it.
    #[test]
    fn content_slower_than_three_periods_never_concludes() {
        assert_eq!(
            run_trial(120, std::iter::repeat_n(33_333, 10_000), 6_000),
            None
        );
    }

    /// An occasional late frame (one in 200, one period late) stays under the
    /// p99 bound.
    #[test]
    fn a_rare_late_frame_is_tolerated() {
        let intervals: Vec<u64> = (0..2_000u64)
            .map(|n| if n % 200 == 199 { 16_000 } else { 8_333 })
            .collect();
        assert_eq!(run_trial(120, intervals, 6_000), Some(Verdict::Proven));
    }

    /// One frame in fifty captured and then dropped for the viewer. Counted,
    /// the two-period gap across each drop is the p99, and a host that kept up
    /// never proves it; as a pause, the trial judges what the host did.
    #[test]
    fn a_frame_dropped_for_the_viewer_breaks_the_stretch() {
        let judge = |pause_on_drop: bool| {
            let mut meter = TrialMeter::new(120);
            let mut at = 1_000_000u64;
            for n in 0..3_000u64 {
                at += 8_333;
                if n % 50 == 49 {
                    // Captured, then dropped by the pump: never encoded.
                    if pause_on_drop {
                        meter.pause();
                    }
                    continue;
                }
                if let Some(v) = meter.frame(at, 6_000) {
                    return Some(v);
                }
            }
            None
        };
        assert_eq!(judge(true), Some(Verdict::Proven));
        assert_eq!(
            judge(false),
            None,
            "the control: counted, the drops keep a host that kept up from proving it"
        );
    }

    /// No push-back is no claim. A pushed-back estimate under the ceiling at
    /// the asked rate refuses.
    #[test]
    fn the_pipe_covers_unless_a_push_back_says_otherwise() {
        assert!(pipe_covers(None, 80_000_000));
        assert!(pipe_covers(Some(80_000_000), 80_000_000));
        assert!(!pipe_covers(Some(20_000_000), 80_000_000));
    }

    #[test]
    fn the_deny_list_parses_like_the_cell_denylist_and_only_removes() {
        let list = parse_fps_deny(" hevc_qsv:120, av1_nvenc:144 ,*:240, broken, :60, hevc_nvenc:x");
        assert_eq!(
            list,
            vec![
                ("hevc_qsv".to_string(), 120),
                ("av1_nvenc".to_string(), 144),
                ("*".to_string(), 240),
            ]
        );
        assert!(fps_denied(&list, "hevc_qsv", 120));
        assert!(!fps_denied(&list, "hevc_qsv", 144));
        assert!(fps_denied(&list, "anything", 240));
        assert!(!fps_denied(&parse_fps_deny(""), "hevc_qsv", 120));
    }

    #[test]
    fn the_new_reasons_have_their_wire_names() {
        assert_eq!(Reason::Refresh.wire(), "refresh");
        assert_eq!(Reason::Denied.wire(), "denied");
        assert_eq!(Reason::Pipe.wire(), "pipe");
    }

    #[test]
    fn only_the_offered_rates_are_an_ask() {
        let ask = |v: serde_json::Value| parse_ask(&v);
        assert_eq!(
            ask(serde_json::json!({"t": "rc:fps", "fps": 120})),
            Some(120)
        );
        assert_eq!(ask(serde_json::json!({"t": "rc:fps", "fps": 60})), Some(60));
        assert_eq!(
            ask(serde_json::json!({"t": "rc:fps", "fps": 144})),
            Some(144)
        );
        // Not offered, not a number, absent, or out of range: dropped.
        assert_eq!(ask(serde_json::json!({"t": "rc:fps", "fps": 90})), None);
        assert_eq!(ask(serde_json::json!({"t": "rc:fps", "fps": "120"})), None);
        assert_eq!(ask(serde_json::json!({"t": "rc:fps"})), None);
        assert_eq!(ask(serde_json::json!({"t": "rc:fps", "fps": -60})), None);
        assert_eq!(
            ask(serde_json::json!({"t": "rc:fps", "fps": 4_294_967_416u64})),
            None
        );
    }

    /// P1 grants nothing above the cap, and names why.
    #[test]
    fn an_ask_above_the_cap_is_refused_with_its_reason() {
        assert_eq!(
            answer(Some(120), 60, false),
            Answer {
                fps: 60,
                refused: Some((120, Reason::Unproven)),
            }
        );
        assert_eq!(
            answer(Some(60), 30, true),
            Answer {
                fps: 30,
                refused: Some((60, Reason::Constrained)),
            }
        );
    }

    /// Nothing to explain when the stream already runs at what was asked, or
    /// faster, or when nobody asked.
    #[test]
    fn an_ask_the_cap_covers_needs_no_explanation() {
        let plain = Answer {
            fps: 60,
            refused: None,
        };
        assert_eq!(answer(Some(60), 60, false), plain);
        assert_eq!(answer(None, 60, false), plain);
        assert_eq!(answer(None, 60, true), plain);
    }

    // ── FR-119 P2b-2 — the session's state ──────────────────────────────

    fn class(encoder: &str, long_edge: u32, fps: u32) -> ClassKey {
        ClassKey {
            backend: "scrap".to_string(),
            refresh_hz: 0,
            encoder: encoder.to_string(),
            long_edge,
            fps,
        }
    }

    fn ask(asked: u32) -> GrantInputs {
        GrantInputs {
            asked: Some(asked),
            cap: 60,
            constrained: false,
            refresh_hz: None,
            rates: [RateFacts::OPEN; CHOICES.len()],
        }
    }

    /// Frames `gap_us` apart, each after `encode_us` in the encoder, for `secs`
    /// of motion; the verdict, if one came.
    fn feed(
        s: &mut RateState,
        at_us: &mut u64,
        gap_us: u64,
        encode_us: u64,
        secs: u64,
    ) -> Option<(ClassKey, Verdict)> {
        for _ in 0..secs * 1_000_000 / gap_us {
            *at_us += gap_us;
            if let Some(v) = s.frame(*at_us, encode_us) {
                return Some(v);
            }
        }
        None
    }

    /// P2b-12 — a 120 taken back on `pipe` is named by the decision at once,
    /// while the viewer's answer still waits for the swap down (it would
    /// contradict the 120 the stream still runs at). The log line reads the
    /// first; before P2b-12 it read the second and printed `refused=None`.
    #[test]
    fn the_decision_names_its_refusal_before_the_swap_down() {
        let k = class("hevc_nvenc", 2560, 120);
        let mut s = RateState::new(60, Vec::new());
        s.decide(ask(120), k.clone());
        s.running_at(120);
        assert_eq!(s.decided_refusal(), None);
        let narrow = at(ask(120), 120, |f| f.pipe_covers = false);
        assert!(s.decide(narrow, k));
        assert_eq!(s.want(), 60);
        assert_eq!(s.decided_refusal(), Some((120, Reason::Pipe)));
        assert_eq!(s.answer().refused, None, "still running at 120");
        s.running_at(60);
        assert_eq!(s.answer().refused, Some((120, Reason::Pipe)));
    }

    /// An unproven class is wanted at once, and the trial starts only when the
    /// stream actually runs at the rate.
    #[test]
    fn the_trial_starts_when_the_stream_runs_at_the_granted_rate() {
        let mut s = RateState::new(60, Vec::new());
        assert!(s.decide(ask(120), class("hevc_videotoolbox", 3840, 120)));
        assert_eq!(s.want(), 120);
        assert!(!s.trial_running(), "the swap has not landed");
        assert_eq!(s.video_info_fields(), r#","fps":60"#);
        s.running_at(120);
        assert!(s.trial_running());
        assert_eq!(s.video_info_fields(), r#","fps":120,"fps_trial":true"#);
        assert!(
            !s.decide(ask(120), class("hevc_videotoolbox", 3840, 120)),
            "nothing moved"
        );
    }

    /// A trial that holds the rate is remembered, and the stream runs on at it
    /// with no trial.
    #[test]
    fn a_proven_trial_runs_on_without_one() {
        let k = class("hevc_videotoolbox", 3840, 120);
        let mut s = RateState::new(60, Vec::new());
        s.decide(ask(120), k.clone());
        s.running_at(120);
        let mut at = 0;
        assert_eq!(
            feed(&mut s, &mut at, 8_333, 6_000, 12),
            Some((k, Verdict::Proven))
        );
        assert_eq!(s.want(), 120);
        assert!(!s.trial_running());
        assert_eq!(s.video_info_fields(), r#","fps":120"#);
    }

    /// P2b-3 — a concluded trial says what it measured: the numbers its verdict
    /// was decided on, which the pump logs beside it.
    #[test]
    fn a_concluded_trial_says_what_it_measured() {
        let k = class("hevc_videotoolbox", 3840, 120);
        let mut s = RateState::new(60, Vec::new());
        assert_eq!(s.concluded(), None);
        s.decide(ask(120), k.clone());
        s.running_at(120);
        let mut at = 0;
        assert_eq!(
            feed(&mut s, &mut at, 8_333, 6_000, 12),
            Some((k, Verdict::Proven))
        );
        // 1201 counted gaps of 8333 µs are the first window past 10 s.
        assert_eq!(
            s.concluded(),
            Some(TrialSummary {
                frames: 1_201,
                active_ms: 10_007,
                fps_x10: 1_200,
                p99_interval_us: 8_333,
                p90_encode_us: 6_000,
            })
        );
    }

    /// A trial that falls short takes the stream back to the cap. The refusal
    /// is told once the stream runs below the ask, not while the swap down is
    /// still opening.
    #[test]
    fn a_failed_trial_goes_back_to_the_cap_and_says_unproven() {
        let k = class("hevc_qsv", 1920, 120);
        let mut s = RateState::new(60, Vec::new());
        s.decide(ask(120), k.clone());
        s.running_at(120);
        let mut at = 0;
        assert_eq!(
            feed(&mut s, &mut at, 11_000, 11_000, 12),
            Some((k, Verdict::Failed))
        );
        assert_eq!(s.want(), 60);
        assert_eq!(s.video_info_fields(), r#","fps":120"#, "still at 120");
        s.running_at(60);
        assert_eq!(
            s.video_info_fields(),
            r#","fps":60,"fps_asked":120,"fps_reason":"unproven""#
        );
    }

    /// What this session concluded holds for the rest of it: a failed class is
    /// not tried again, and another class is.
    #[test]
    fn a_class_this_session_failed_is_not_tried_again() {
        let mut s = RateState::new(60, Vec::new());
        s.decide(ask(120), class("hevc_qsv", 1920, 120));
        s.running_at(120);
        let mut at = 0;
        assert!(feed(&mut s, &mut at, 11_000, 11_000, 12).is_some());
        s.running_at(60);
        s.decide(ask(120), class("hevc_qsv", 1920, 120));
        assert_eq!(s.want(), 60);
        s.decide(ask(120), class("av1_qsv", 1920, 120));
        assert_eq!(s.want(), 120, "another encoder is another class");
    }

    /// The cache's verdicts decide from the first ask.
    #[test]
    fn a_cached_verdict_decides_from_the_start() {
        let k = class("hevc_nvenc", 2560, 120);
        let mut s = RateState::new(60, vec![(k.clone(), Verdict::Proven)]);
        s.decide(ask(120), k.clone());
        s.running_at(120);
        assert!(!s.trial_running(), "proven: no trial");
        let mut s = RateState::new(60, vec![(k.clone(), Verdict::Failed)]);
        s.decide(ask(120), k);
        assert_eq!(s.want(), 60);
        assert_eq!(
            s.video_info_fields(),
            r#","fps":60,"fps_asked":120,"fps_reason":"unproven""#
        );
    }

    /// P2b-17b — a retry forgets the session's verdict for the class asked for,
    /// so a class refused as unproven is granted again, as a trial. Without the
    /// retry, or for another class, nothing moves.
    #[test]
    fn a_retry_trials_a_failed_class_again() {
        let k = class("av1_nvenc", 2560, 144);
        let other = class("hevc_nvenc", 2560, 144);
        let mut s = RateState::new(
            60,
            vec![
                (k.clone(), Verdict::Failed),
                (k.at(120), Verdict::Failed),
                (other.clone(), Verdict::Failed),
                (other.at(120), Verdict::Failed),
            ],
        );
        s.decide_with_retry(ask(144), k.clone(), false);
        assert_eq!(s.want(), 60, "no retry: still refused");
        assert!(
            s.decide_with_retry(ask(144), k.clone(), true),
            "the retry changes the decision"
        );
        assert_eq!(s.want(), 144, "granted again");
        s.running_at(144);
        assert!(s.trial_running(), "as a trial");
        s.decide_with_retry(ask(144), other, false);
        assert_eq!(s.want(), 60, "another encoder keeps its verdicts");
    }

    /// P2b-18 — a trial that falls short steps down one rate, and the next
    /// rate gets a trial of its own: 144, then 120, then the cap. Each verdict
    /// is handed over for the cache under its own rate.
    #[test]
    fn a_failed_trial_steps_down_one_rate_at_a_time() {
        let k = class("hevc_nvenc", 2560, 144);
        let mut s = RateState::new(60, Vec::new());
        s.decide(ask(144), k.clone());
        s.running_at(144);
        assert!(s.trial_running());
        let mut at_us = 0;
        // 144's period is 6.9 ms: frames 8 ms apart (125 fps, under 90 % of
        // 144), each 8 ms in the encoder, is a host that cannot keep up.
        assert_eq!(
            feed(&mut s, &mut at_us, 8_000, 8_000, 12),
            Some((k.clone(), Verdict::Failed))
        );
        assert_eq!(s.want(), 120, "down one rate, not to the cap");
        assert_eq!(
            s.video_info_fields(),
            r#","fps":144"#,
            "still at 144 while the swap down opens"
        );
        s.running_at(120);
        assert!(s.trial_running(), "120 was never tried: its own trial");
        assert_eq!(
            s.video_info_fields(),
            r#","fps":120,"fps_asked":144,"fps_reason":"unproven","fps_trial":true"#
        );
        // 120's period is 8.3 ms: 9.5 ms frames (105 fps) do not fit it
        // either.
        assert_eq!(
            feed(&mut s, &mut at_us, 9_500, 9_500, 12),
            Some((k.at(120), Verdict::Failed))
        );
        assert_eq!(s.want(), 60, "then the cap");
        s.running_at(60);
        assert!(!s.trial_running());
        assert_eq!(
            s.video_info_fields(),
            r#","fps":60,"fps_asked":144,"fps_reason":"unproven""#
        );
    }

    /// P2b-18 — a 120 that proves itself after a failed 144 is where the
    /// stream stays, with no further trial, and the viewer reads why it is
    /// not at 144. A new session starts there at once from the cache.
    #[test]
    fn a_proven_step_down_runs_on_at_that_rate() {
        let k = class("hevc_nvenc", 2560, 144);
        let mut s = RateState::new(60, vec![(k.clone(), Verdict::Failed)]);
        s.decide(ask(144), k.clone());
        assert_eq!(s.want(), 120);
        s.running_at(120);
        let mut at_us = 0;
        assert_eq!(
            feed(&mut s, &mut at_us, 8_333, 6_000, 12),
            Some((k.at(120), Verdict::Proven))
        );
        assert_eq!(s.want(), 120);
        assert!(!s.trial_running());
        assert_eq!(
            s.video_info_fields(),
            r#","fps":120,"fps_asked":144,"fps_reason":"unproven""#
        );
        let mut next = RateState::new(
            60,
            vec![(k.clone(), Verdict::Failed), (k.at(120), Verdict::Proven)],
        );
        next.decide(ask(144), k);
        assert_eq!(next.want(), 120);
        next.running_at(120);
        assert!(!next.trial_running(), "proven: no trial");
    }

    /// P2b-18 — "try 144 again" walks the whole step down again: a 120 that
    /// failed earlier gets a fresh trial if 144 falls short once more, while
    /// a proven 120 stays proven.
    #[test]
    fn a_retry_walks_the_step_down_again_from_the_top() {
        let k = class("hevc_nvenc", 2560, 144);
        let mut s = RateState::new(
            60,
            vec![(k.clone(), Verdict::Failed), (k.at(120), Verdict::Failed)],
        );
        s.decide_with_retry(ask(144), k.clone(), false);
        assert_eq!(s.want(), 60, "both failed: the cap");
        s.decide_with_retry(ask(144), k.clone(), true);
        assert_eq!(s.want(), 144);
        s.running_at(144);
        let mut at_us = 0;
        assert_eq!(
            feed(&mut s, &mut at_us, 8_000, 8_000, 12),
            Some((k.clone(), Verdict::Failed))
        );
        assert_eq!(s.want(), 120, "120's earlier failure was forgotten too");
        s.running_at(120);
        assert!(s.trial_running(), "so it is trialled again");

        let mut s = RateState::new(
            60,
            vec![(k.clone(), Verdict::Failed), (k.at(120), Verdict::Proven)],
        );
        s.decide_with_retry(ask(144), k.clone(), true);
        s.running_at(144);
        let mut at_us = 0;
        assert_eq!(
            feed(&mut s, &mut at_us, 8_000, 8_000, 12),
            Some((k.clone(), Verdict::Failed))
        );
        assert_eq!(s.want(), 120);
        s.running_at(120);
        assert!(!s.trial_running(), "a proven 120 runs without a trial");
    }

    /// P2b-17b — only a literal `retry: true` asks to trial again, and the two
    /// verbs keep their wire spellings.
    #[test]
    fn only_a_literal_true_retries() {
        let retry = |v: serde_json::Value| parse_retry(&v);
        assert!(retry(
            serde_json::json!({"t": "rc:fps", "fps": 144, "retry": true})
        ));
        assert!(!retry(serde_json::json!({"t": "rc:fps", "fps": 144})));
        assert!(!retry(
            serde_json::json!({"t": "rc:fps", "fps": 144, "retry": false})
        ));
        assert!(!retry(
            serde_json::json!({"t": "rc:fps", "fps": 144, "retry": "true"})
        ));
        assert!(!retry(
            serde_json::json!({"t": "rc:fps", "fps": 144, "retry": 1})
        ));
        assert_eq!(CAPS, ["fps-high", "fps-retry"]);
    }

    /// A new class while a trial runs (the geometry moved to another bucket)
    /// starts the trial over: five seconds on the old class count nothing.
    #[test]
    fn a_new_class_starts_the_trial_over() {
        let mut s = RateState::new(60, Vec::new());
        s.decide(ask(120), class("hevc_videotoolbox", 1920, 120));
        s.running_at(120);
        let mut at = 0;
        assert_eq!(feed(&mut s, &mut at, 8_333, 6_000, 5), None);
        let wide = class("hevc_videotoolbox", 3840, 120);
        s.decide(ask(120), wide.clone());
        assert_eq!(
            feed(&mut s, &mut at, 8_333, 6_000, 7),
            None,
            "seven seconds anew"
        );
        assert_eq!(
            feed(&mut s, &mut at, 8_333, 6_000, 5),
            Some((wide, Verdict::Proven))
        );
    }

    /// A replacement at the granted rate that fails to open fails the class
    /// for the session, `unproven`, and nothing is handed over for the cache.
    /// P2b-18: the failed rate is the granted one, and the grant steps down to
    /// the next (144, then 120, then the cap). With nothing granted above the
    /// cap, a failed open fails nothing.
    #[test]
    fn a_failed_open_fails_the_class_for_the_session_only() {
        let k = class("hevc_nvenc", 1920, 144);
        let mut s = RateState::new(60, Vec::new());
        s.decide(ask(144), k.clone());
        assert_eq!(s.want(), 144);
        assert!(s.open_failed());
        assert_eq!(s.want(), 120, "144 did not open: 120 next");
        assert!(s.open_failed());
        assert_eq!(s.want(), 60, "nor did 120: the cap");
        assert_eq!(
            s.video_info_fields(),
            r#","fps":60,"fps_asked":144,"fps_reason":"unproven""#
        );
        s.decide(ask(144), k);
        assert_eq!(s.want(), 60, "not tried again this session");
        assert!(!s.open_failed(), "nothing above the cap to fail");
        let mut s = RateState::new(60, Vec::new());
        s.decide(ask(60), class("hevc_nvenc", 1920, 60));
        assert!(!s.open_failed());
    }

    /// An ask the cap covers wants the cap, with no trial and nothing to
    /// explain, whatever ran before.
    #[test]
    fn an_ask_the_cap_covers_wants_the_cap() {
        let mut s = RateState::new(120, Vec::new());
        s.decide(ask(60), class("hevc_videotoolbox", 3840, 60));
        assert_eq!(s.want(), 60);
        s.running_at(60);
        assert_eq!(s.video_info_fields(), r#","fps":60"#);
    }

    /// The wire, byte for byte: the viewer parses these names.
    #[test]
    fn the_video_info_fields_are_what_the_viewer_reads() {
        assert_eq!(video_info_fields(&answer(None, 60, false)), r#","fps":60"#);
        assert_eq!(
            video_info_fields(&answer(Some(120), 60, false)),
            r#","fps":60,"fps_asked":120,"fps_reason":"unproven""#
        );
        assert_eq!(
            video_info_fields(&answer(Some(144), 30, true)),
            r#","fps":30,"fps_asked":144,"fps_reason":"constrained""#
        );
        assert_eq!(CAP, "fps-high");
    }

    /// FR-119 P2b-3 — the screen's refresh rate rides beside the answer, and
    /// an unknown one says nothing.
    #[test]
    fn the_refresh_rate_field_is_what_the_viewer_reads() {
        assert_eq!(refresh_field(Some(120)), r#","refresh_hz":120"#);
        assert_eq!(refresh_field(None), "");
    }
}
