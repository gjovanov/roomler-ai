// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3b-1 — a device's knowhow scan: the device's half of
//! `rc:knowhow.scan` (the FR-90 spec, §3e "P4-3b, a device's scan, as
//! designed"; `docs/knowhow.md` §11.2).
//!
//! The server asks; the device reads the roots its owner lists in
//! `knowhow_scan_roots` (decision 38), each through a child running as the
//! account that owns it, and answers what it found, checked as the server
//! will check it. Nothing is read that no root names, and nothing as root.
//!
//! | Piece | Where |
//! |---|---|
//! | the frame: on the primary's connection only, a scan id run once | [`handle_scan`] |
//! | the roots, read from the config file at each scan | [`Scanner::scan`] |
//! | whose eyes: the root's owner, never uid 0, never one a link chose | [`owner`] |
//! | the child, `roomlerd knowhow-scan`, as that account | [`scan_main`] |
//! | the walk and its bounds | [`walk`] |
//! | a checkout, read as text | [`checkout`] |
//! | the answer | [`assemble`] |
//!
//! ⚠️ Only a scan that read everything it lists retires anything. A root
//! refused, unreadable, cut or ended makes the answer `truncated`, which the
//! server reads as "archive nothing" (`docs/knowhow.md` §11.1): a folder on an
//! unplugged disk, or a typo, must never archive what earlier scans found.

mod checkout;
mod owner;
mod walk;

use std::collections::{HashSet, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bson::oid::ObjectId;
use roomler_ai_remote_control::knowhow::{
    Finding, MAX_DETAIL, MAX_FINDINGS, MAX_SCAN_BYTES, ScanOutcome, TunnelRoute,
};
use roomler_ai_remote_control::signaling::ClientMsg;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::checkpointer::Runner;

/// The hidden subcommand: `roomlerd knowhow-scan`, its request on stdin.
pub const SCAN_SUBCOMMAND: &str = "knowhow-scan";

/// Bumped when a field changes its meaning; a child refuses another.
const REQUEST_V: u32 = 1;
/// The most request bytes a child reads.
const MAX_REQUEST: usize = 16 * 1024;
/// The most roots one scan reads; the rest are reported unread.
const MAX_ROOTS: usize = 32;
/// How long one root's walk may take: the child keeps it itself, and answers
/// what it found by then as cut.
const ROOT_DEADLINE: Duration = Duration::from_secs(60);
/// How long past its own deadline a child may run before it is ended whole.
const CHILD_GRACE: Duration = Duration::from_secs(15);
/// How long a whole scan may take; the roots it has not reached by then are
/// unread.
const SCAN_DEADLINE: Duration = Duration::from_secs(300);
/// How deep below a root the walk reaches.
const MAX_DEPTH: usize = 8;
/// How many entries a root's walk reads before it is cut.
const MAX_ENTRIES: usize = 100_000;
/// Scans asked for while one runs, at most; past it, a scan is refused.
const MAX_QUEUED: usize = 4;
/// Answers kept, so a scan delivered again is answered, never run again.
const ANSWERS_KEPT: usize = 16;

/// What the daemon asks a child to read: one root, resolved, and its bounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Request {
    pub v: u32,
    pub root: PathBuf,
    pub deadline_ms: u64,
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_findings: usize,
    pub max_bytes: usize,
}

impl Request {
    fn new(root: PathBuf, deadline: Duration) -> Self {
        Self {
            v: REQUEST_V,
            root,
            deadline_ms: u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX),
            max_depth: MAX_DEPTH,
            max_entries: MAX_ENTRIES,
            max_findings: MAX_FINDINGS,
            max_bytes: MAX_SCAN_BYTES,
        }
    }
}

/// What a child found under its root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Found {
    pub v: u32,
    /// Each finding as the wire carries it ([`Finding::to_value`]).
    pub findings: Vec<Value>,
    /// The walk stopped at a bound or at its deadline: what it found is not
    /// all there is.
    pub cut: bool,
    /// The entries it read.
    pub entries: usize,
}

// ─── the child ──────────────────────────────────────────────────────────────

/// Whether this process was started as `roomlerd knowhow-scan`, its request
/// on stdin.
pub fn scan_args() -> bool {
    let mut args = std::env::args_os().skip(1);
    args.next().is_some_and(|a| a == SCAN_SUBCOMMAND) && args.next().is_none()
}

/// The child: read one root as whoever this process runs as (the root's
/// owner), and write what it found to stdout. 0 on success; 1 with a word on
/// stderr otherwise. Never as root: the daemon drops to the root's owner
/// before this runs, and a child that finds itself root refuses.
pub fn scan_main() -> i32 {
    let run = || -> Result<(), String> {
        // SAFETY: reads the caller's own credentials; it cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return Err("a knowhow scan never reads as root".into());
        }
        let mut text = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(std::io::stdin().lock(), MAX_REQUEST as u64 + 1),
            &mut text,
        )
        .map_err(|e| format!("reading the request: {e}"))?;
        if text.len() > MAX_REQUEST {
            return Err("the request is too large".into());
        }
        let req: Request =
            serde_json::from_slice(&text).map_err(|e| format!("the request: {e}"))?;
        if req.v != REQUEST_V {
            return Err(format!(
                "a request of version {}, and this binary reads {REQUEST_V}",
                req.v
            ));
        }
        let found = produce(&req)?;
        let line = serde_json::to_string(&found).map_err(|e| e.to_string())?;
        println!("{line}");
        Ok(())
    };
    match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("roomlerd {SCAN_SUBCOMMAND}: {e}");
            1
        }
    }
}

/// Read `req.root` as this process's own account: the child's work, and the
/// in-process runner's in tests. The root must still be a folder this
/// account owns: the daemon resolved it, and it may have changed since.
pub(crate) fn produce(req: &Request) -> Result<Found, String> {
    let deadline = Instant::now() + Duration::from_millis(req.deadline_ms);
    let root =
        std::fs::canonicalize(&req.root).map_err(|e| format!("{}: {e}", req.root.display()))?;
    let meta = std::fs::metadata(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    // SAFETY: reads the caller's own credentials; it cannot fail.
    let me = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != me {
        return Err(format!(
            "{} is no longer a folder this account owns",
            root.display()
        ));
    }
    // A root the account cannot list is unread, never an empty folder: its
    // mode, or macOS's privacy protections without Full Disk Access, would
    // otherwise make a whole scan of nothing, and the server would archive
    // everything it held.
    std::fs::read_dir(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    let walked = walk::walk(
        &root,
        &walk::Bounds {
            max_depth: req.max_depth,
            max_entries: req.max_entries,
            deadline,
        },
    );
    let git = checkout::find_git();
    let mut found = Found {
        v: REQUEST_V,
        findings: Vec::new(),
        cut: walked.cut,
        entries: walked.entries,
    };
    let mut bytes = 0;
    for dir in &walked.checkouts {
        if Instant::now() >= deadline {
            found.cut = true;
            break;
        }
        let Some(c) = checkout::read(dir, git.as_deref(), deadline) else {
            continue;
        };
        let value = Finding::Checkout(c).to_value();
        let size = value.to_string().len() + 1;
        if found.findings.len() >= req.max_findings || bytes + size > req.max_bytes {
            found.cut = true;
            break;
        }
        bytes += size;
        found.findings.push(value);
    }
    Ok(found)
}

// ─── the daemon ─────────────────────────────────────────────────────────────

/// The device's scanner: where its roots are read from, how a root is read,
/// and the scans it is running, has waiting and has answered. Held by the
/// primary enrollment's context alone (decision 43).
pub struct Scanner {
    config: PathBuf,
    lock: crate::config::WriteLock,
    run: Runner,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The scan being run, and the connection to answer it on: the newest
    /// that delivered it.
    running: Option<(ObjectId, mpsc::Sender<ClientMsg>)>,
    queued: VecDeque<(ObjectId, mpsc::Sender<ClientMsg>)>,
    answered: VecDeque<(ObjectId, ClientMsg)>,
}

/// A root a scan did not read, and why.
#[derive(Debug)]
struct Unread {
    root: String,
    why: String,
}

impl Scanner {
    fn new(config: PathBuf, lock: crate::config::WriteLock, run: Runner) -> Self {
        Self {
            config,
            lock,
            run,
            state: Mutex::new(State::default()),
        }
    }

    /// The daemon's: roots read from `config` under `lock` (the LocalAPI's
    /// own, so a scan never reads a half-written file), each by a child as
    /// its owner.
    pub fn subprocess(config: PathBuf, lock: crate::config::WriteLock) -> Arc<Self> {
        Arc::new(Self::new(config, lock, Runner::Subprocess))
    }

    /// An integration test's device, whose binary is a test's and knows no
    /// `knowhow-scan`: each root is read in this process, as it.
    #[cfg(feature = "hive-test-launcher")]
    pub fn in_process(config: PathBuf, lock: crate::config::WriteLock) -> Arc<Self> {
        let ceiling = std::env::temp_dir();
        Arc::new(Self::new(config, lock, Runner::InProcess { ceiling }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `scan_id`, or answer it from what it found, or keep it for later.
    fn request(self: Arc<Self>, scan_id: ObjectId, tx: mpsc::Sender<ClientMsg>) {
        let mut st = self.state();
        if let Some((_, answer)) = st.answered.iter().find(|(id, _)| *id == scan_id) {
            debug!(%scan_id, "knowhow: a scan delivered again, answered from what it found");
            send(tx, answer.clone());
            return;
        }
        match st.running.as_ref().map(|(id, _)| *id) {
            Some(id) if id == scan_id => {
                // Delivered again on a newer connection: answer it there.
                if let Some(running) = st.running.as_mut() {
                    running.1 = tx;
                }
                return;
            }
            Some(_) => {
                if let Some(waiting) = st.queued.iter_mut().find(|(id, _)| *id == scan_id) {
                    waiting.1 = tx;
                } else if st.queued.len() >= MAX_QUEUED {
                    drop(st);
                    send(tx, failed(scan_id, "this device is busy with other scans"));
                } else {
                    st.queued.push_back((scan_id, tx));
                }
                return;
            }
            None => {}
        }
        st.running = Some((scan_id, tx));
        drop(st);
        tokio::spawn(self.drive());
    }

    /// Run the scan in hand and answer it, then the next, until none waits.
    async fn drive(self: Arc<Self>) {
        loop {
            let Some(scan_id) = self.state().running.as_ref().map(|(id, _)| *id) else {
                return;
            };
            let answer = self.scan(scan_id).await;
            let (tx, more) = {
                let mut st = self.state();
                let tx = st.running.take().map(|(_, tx)| tx);
                st.answered.push_back((scan_id, answer.clone()));
                while st.answered.len() > ANSWERS_KEPT {
                    st.answered.pop_front();
                }
                st.running = st.queued.pop_front();
                (tx, st.running.is_some())
            };
            // A connection that died meanwhile drops the answer; the server
            // delivers the scan again at the next hello, and it is answered
            // from `answered`.
            if let Some(tx) = tx {
                let _ = tx.send(answer).await;
            }
            if !more {
                return;
            }
        }
    }

    /// One scan: the roots read from the config file now, each read as its
    /// owner, then the answer.
    async fn scan(&self, scan_id: ObjectId) -> ClientMsg {
        let cfg = match self.read_config().await {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(%scan_id, error = %e, "knowhow: the config could not be read");
                return failed(scan_id, &format!("reading its config: {e}"));
            }
        };
        let roots = &cfg.knowhow_scan_roots;
        if roots.is_empty() {
            info!(%scan_id, "knowhow: no roots listed, answered no_roots");
            return ClientMsg::KnowhowScanned {
                scan_id,
                outcome: ScanOutcome::NoRoots,
                detail: None,
                truncated: false,
                findings: Vec::new(),
            };
        }
        let started = Instant::now();
        // SAFETY: reads the caller's own credentials; it cannot fail.
        let daemon_uid = unsafe { libc::geteuid() };
        let mut found = Vec::new();
        let mut unread = Vec::new();
        for (i, listed) in roots.iter().enumerate() {
            let left = SCAN_DEADLINE.saturating_sub(started.elapsed());
            let read = if i >= MAX_ROOTS {
                Err(format!("past the first {MAX_ROOTS} roots"))
            } else if left < Duration::from_secs(1) {
                Err("the scan's time ran out".to_string())
            } else {
                match owner::prepare(Path::new(listed), daemon_uid) {
                    Ok(root) => self.read_root(&root, ROOT_DEADLINE.min(left)).await,
                    Err(why) => Err(why),
                }
            };
            match read {
                Ok(f) => found.push(f),
                Err(why) => {
                    info!(%scan_id, root = %listed, %why, "knowhow: a root not read");
                    unread.push(Unread {
                        root: listed.clone(),
                        why,
                    });
                }
            }
        }
        let routes = declared_routes(&cfg.tunnel_routes);
        let answer = assemble(scan_id, roots.len(), found, routes, &unread);
        if let ClientMsg::KnowhowScanned {
            outcome,
            truncated,
            findings,
            ..
        } = &answer
        {
            info!(
                %scan_id,
                outcome = outcome.as_str(),
                truncated,
                findings = findings.len(),
                unread = unread.len(),
                "knowhow: scanned"
            );
        }
        answer
    }

    async fn read_config(&self) -> Result<crate::config::AgentConfig, String> {
        let _guard = self.lock.lock().await;
        let path = self.config.clone();
        tokio::task::spawn_blocking(move || {
            crate::config::load(&path).map_err(|e| format!("{e:#}"))
        })
        .await
        .map_err(|e| format!("the read's task: {e}"))?
    }

    /// One root, read through the runner: a child as its owner on a device,
    /// in this process in tests.
    async fn read_root(&self, root: &owner::Root, deadline: Duration) -> Result<Found, String> {
        let req = Request::new(root.path.clone(), deadline);
        match &self.run {
            Runner::Subprocess => {
                run_child(&req, root.account.as_deref(), deadline + CHILD_GRACE).await
            }
            #[cfg(any(test, feature = "hive-test-launcher"))]
            Runner::InProcess { .. } => tokio::task::spawn_blocking(move || produce(&req))
                .await
                .map_err(|e| format!("the read's task: {e}"))?,
            #[cfg(test)]
            Runner::Failing(why) => Err((*why).to_string()),
            #[cfg(test)]
            Runner::Hangs => {
                tokio::time::sleep(deadline + CHILD_GRACE).await;
                Err("ended at its deadline".into())
            }
        }
    }
}

/// The daemon's side: start `roomlerd knowhow-scan` as `account` (none: as
/// the daemon, which is then the root's owner), its environment emptied but
/// for that account's, in a process group of its own that the deadline ends
/// whole, and feed it `req` on stdin.
async fn run_child(req: &Request, account: Option<&str>, limit: Duration) -> Result<Found, String> {
    use super::child::Fed;
    use std::process::Stdio;
    let exe = PathBuf::from(super::supervisor::own_exe()?);
    let bytes = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let mut cmd = tokio::process::Command::new(&exe);
    cmd.arg(SCAN_SUBCOMMAND)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    match account {
        // `HOME`, `USER` and `LOGNAME` become the account's, its
        // administrator groups are shed, and on Linux it gains no privilege
        // by exec: the drop an agent session takes.
        Some(account) => crate::exec::apply_session_run_as(&mut cmd, account)?,
        None => {
            if let Some(home) = std::env::var_os("HOME") {
                cmd.env("HOME", home);
            }
        }
    }
    // SAFETY: one syscall over no memory, after the drop: a scan yields to
    // the person's own work, and lowering one's own priority needs no
    // privilege.
    unsafe {
        cmd.pre_exec(|| {
            libc::nice(10);
            Ok(())
        })
    };
    let feed = |mut stdin: tokio::process::ChildStdin| async move {
        // `stdin` is dropped at the end: the child's end of input.
        match tokio::io::AsyncWriteExt::write_all(&mut stdin, &bytes).await {
            Ok(()) => Fed::Done,
            Err(_) => Fed::Closed,
        }
    };
    let max = (MAX_SCAN_BYTES + 64 * 1024) as u64;
    let out = super::child::run_fed(cmd, SCAN_SUBCOMMAND, feed, limit, max).await?;
    let found: Found =
        serde_json::from_slice(&out).map_err(|e| format!("the scan's answer: {e}"))?;
    if found.v != REQUEST_V {
        return Err("an answer of another version".into());
    }
    Ok(found)
}

/// `rc:knowhow.scan`: run it, or answer why not. Never awaited by the
/// signaling loop: a scan takes minutes, and a loop that waited on one would
/// hold every frame behind it.
pub fn handle_scan(
    scanner: Option<&Arc<Scanner>>,
    scan_id: ObjectId,
    is_primary: bool,
    tx: mpsc::Sender<ClientMsg>,
) {
    if !is_primary {
        // Decision 43: the roots are one device-owned list, read for the
        // primary enrollment alone. The server refuses to ask (409); this
        // holds should it ask anyway.
        warn!(%scan_id, "rc:knowhow.scan refused — not the primary enrollment");
        send(
            tx,
            failed(
                scan_id,
                "this device scans for its primary organization only",
            ),
        );
        return;
    }
    let Some(scanner) = scanner else {
        warn!(%scan_id, "rc:knowhow.scan refused — this daemon reads no config file");
        send(
            tx,
            failed(
                scan_id,
                "this daemon has no config file to read its roots from",
            ),
        );
        return;
    };
    Arc::clone(scanner).request(scan_id, tx);
}

fn send(tx: mpsc::Sender<ClientMsg>, msg: ClientMsg) {
    tokio::spawn(async move {
        let _ = tx.send(msg).await;
    });
}

fn failed(scan_id: ObjectId, why: &str) -> ClientMsg {
    ClientMsg::KnowhowScanned {
        scan_id,
        outcome: ScanOutcome::Failed,
        detail: Some(clamp(why)),
        truncated: false,
        findings: Vec::new(),
    }
}

/// The primary's declared tunnel routes (`[[tunnel_routes]]`), as findings:
/// a secondary's names another organization's node, which this map must
/// never learn.
fn declared_routes(routes: &[tunnel_core::localapi::RouteDescriptor]) -> Vec<TunnelRoute> {
    use tunnel_core::localapi::FlowKind;
    routes
        .iter()
        .filter(|r| r.org.as_deref().is_none_or(|org| org == "primary"))
        .map(|r| TunnelRoute {
            id: r.id.clone(),
            flow: match r.kind {
                FlowKind::Forward => "forward",
                FlowKind::Socks5 => "socks5",
            }
            .to_string(),
            local: r.local,
            node: r.node.clone(),
            remote: r.remote.clone(),
            enabled: r.enabled,
        })
        .collect()
}

/// The answer: the declared routes, then each root's findings, each checked
/// as the server will check it and sent once, within the answer's bounds.
/// `failed` when no root was read, so the server writes and archives
/// nothing; `truncated` when a root went unread or was cut, so it archives
/// nothing.
fn assemble(
    scan_id: ObjectId,
    listed: usize,
    found: Vec<Found>,
    routes: Vec<TunnelRoute>,
    unread: &[Unread],
) -> ClientMsg {
    if found.is_empty() {
        return failed(scan_id, &describe(listed, unread, 0));
    }
    let mut truncated = !unread.is_empty() || found.iter().any(|f| f.cut);
    let candidates = routes
        .into_iter()
        .map(|r| Finding::TunnelRoute(r).to_value())
        .chain(found.into_iter().flat_map(|f| f.findings));
    let mut findings = Vec::new();
    let mut seen = HashSet::new();
    let (mut bytes, mut dropped) = (0usize, 0usize);
    for value in candidates {
        let key = match Finding::read(&value) {
            Ok(Some(Finding::Checkout(c))) => format!("checkout {}", c.path),
            Ok(Some(Finding::TunnelRoute(r))) => format!("tunnel_route {}", r.id),
            Ok(Some(_)) => value.to_string(),
            // A kind this build cannot read, or one the server would refuse.
            Ok(None) | Err(_) => {
                dropped += 1;
                continue;
            }
        };
        if !seen.insert(key) {
            continue;
        }
        let size = value.to_string().len() + 1;
        if findings.len() >= MAX_FINDINGS || bytes + size > MAX_SCAN_BYTES {
            truncated = true;
            break;
        }
        bytes += size;
        findings.push(value);
    }
    let detail = describe(listed, unread, dropped);
    ClientMsg::KnowhowScanned {
        scan_id,
        outcome: ScanOutcome::Done,
        detail: (!detail.is_empty()).then_some(detail),
        truncated,
        findings,
    }
}

/// `detail`: the roots not read and why, and the findings dropped.
fn describe(listed: usize, unread: &[Unread], dropped: usize) -> String {
    let mut parts = Vec::new();
    if !unread.is_empty() {
        let each: Vec<String> = unread
            .iter()
            .map(|u| format!("{} ({})", u.root, u.why))
            .collect();
        parts.push(format!(
            "{} of {listed} roots not read: {}",
            unread.len(),
            each.join("; ")
        ));
    }
    if dropped > 0 {
        parts.push(format!("{dropped} findings dropped"));
    }
    clamp(&parts.join("; "))
}

/// At most [`MAX_DETAIL`] characters, an ellipsis where it was cut.
fn clamp(s: &str) -> String {
    if s.chars().count() <= MAX_DETAIL {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_DETAIL - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use roomler_ai_remote_control::knowhow::Checkout;
    use tunnel_core::localapi::{FlowKind, RouteDescriptor};

    fn value(path: &str) -> Value {
        Finding::Checkout(Checkout {
            path: path.into(),
            remotes: Vec::new(),
            root_commit: None,
            branch: None,
            services: Vec::new(),
        })
        .to_value()
    }

    fn found(paths: &[&str], cut: bool) -> Found {
        Found {
            v: REQUEST_V,
            findings: paths.iter().map(|p| value(p)).collect(),
            cut,
            entries: paths.len(),
        }
    }

    fn unread(root: &str, why: &str) -> Unread {
        Unread {
            root: root.into(),
            why: why.into(),
        }
    }

    fn route(id: &str, org: Option<&str>) -> RouteDescriptor {
        RouteDescriptor {
            id: id.into(),
            kind: FlowKind::Forward,
            node: ObjectId::new().to_hex(),
            local: 15432,
            remote: Some("db.internal:5432".into()),
            transport: String::new(),
            enabled: true,
            org: org.map(str::to_string),
        }
    }

    fn parts(msg: &ClientMsg) -> (ScanOutcome, Option<String>, bool, Vec<Value>) {
        let ClientMsg::KnowhowScanned {
            outcome,
            detail,
            truncated,
            findings,
            ..
        } = msg
        else {
            panic!("not a scan's answer: {msg:?}");
        };
        (*outcome, detail.clone(), *truncated, findings.clone())
    }

    /// Every root read whole: `done`, not truncated, the routes first and a
    /// checkout two roots share sent once.
    #[test]
    fn a_whole_scan_is_done_its_findings_once() {
        let id = ObjectId::new();
        let routes = declared_routes(&[route("db", None)]);
        let msg = assemble(
            id,
            2,
            vec![found(&["/a/x", "/a/y"], false), found(&["/a/y"], false)],
            routes,
            &[],
        );
        let (outcome, detail, truncated, findings) = parts(&msg);
        assert_eq!(outcome, ScanOutcome::Done);
        assert!(!truncated);
        assert_eq!(detail, None);
        let kinds: Vec<&str> = findings
            .iter()
            .map(|f| f["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["tunnel_route", "checkout", "checkout"]);
    }

    /// A root unread or cut makes the answer `truncated`, so nothing earlier
    /// scans found is archived; no root read at all is `failed`.
    #[test]
    fn only_a_scan_that_read_everything_retires_anything() {
        let id = ObjectId::new();
        let msg = assemble(
            id,
            2,
            vec![found(&["/a/x"], false)],
            Vec::new(),
            &[unread("/b", "missing")],
        );
        let (outcome, detail, truncated, findings) = parts(&msg);
        assert_eq!(outcome, ScanOutcome::Done);
        assert!(truncated, "a root went unread");
        assert_eq!(findings.len(), 1);
        assert_eq!(
            detail.as_deref(),
            Some("1 of 2 roots not read: /b (missing)")
        );

        let (_, _, truncated, _) = parts(&assemble(
            id,
            1,
            vec![found(&["/a/x"], true)],
            Vec::new(),
            &[],
        ));
        assert!(truncated, "a root was cut");

        let msg = assemble(
            id,
            1,
            Vec::new(),
            declared_routes(&[route("db", None)]),
            &[unread("/b", "owned by root")],
        );
        let (outcome, detail, truncated, findings) = parts(&msg);
        assert_eq!(outcome, ScanOutcome::Failed, "no root read");
        assert!(findings.is_empty(), "nothing written, the routes included");
        assert!(!truncated);
        assert!(detail.unwrap().contains("owned by root"));
    }

    /// The answer's bounds: at most `MAX_FINDINGS`, then `truncated`; a
    /// finding the server would refuse dropped and counted.
    #[test]
    fn the_answer_keeps_its_bounds_and_drops_what_the_server_would_refuse() {
        let id = ObjectId::new();
        let many: Vec<String> = (0..=MAX_FINDINGS).map(|i| format!("/r/{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let (_, _, truncated, findings) =
            parts(&assemble(id, 1, vec![found(&refs, false)], Vec::new(), &[]));
        assert_eq!(findings.len(), MAX_FINDINGS);
        assert!(truncated);

        let shaped = format!("/r/{}{}", ["gh", "p_"].concat(), "a1B2".repeat(9));
        let (outcome, detail, truncated, findings) = parts(&assemble(
            id,
            1,
            vec![found(&["/r/ok", &shaped], false)],
            Vec::new(),
            &[],
        ));
        assert_eq!(outcome, ScanOutcome::Done);
        assert_eq!(findings.len(), 1);
        assert!(!truncated, "a dropped finding is not a cut");
        assert_eq!(detail.as_deref(), Some("1 findings dropped"));
    }

    /// A secondary's route names another organization's node: left out.
    #[test]
    fn only_the_primarys_declared_routes_are_reported() {
        let routes = declared_routes(&[
            route("a", None),
            route("b", Some("primary")),
            route("c", Some("acme")),
        ]);
        let ids: Vec<&str> = routes.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(routes[0].flow, "forward");
        assert_eq!(routes[0].remote.as_deref(), Some("db.internal:5432"));
    }

    #[test]
    fn a_detail_is_clamped() {
        let long = "x".repeat(MAX_DETAIL * 2);
        let c = clamp(&long);
        assert_eq!(c.chars().count(), MAX_DETAIL);
        assert!(c.ends_with('…'));
    }

    /// A device whose config lists `roots`, its scanner reading in this
    /// process, and the config file it reads.
    fn scanner(roots: &[&Path]) -> (Arc<Scanner>, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config.toml");
        write_roots(&config, roots);
        let lock: crate::config::WriteLock = Arc::new(tokio::sync::Mutex::new(()));
        let run = Runner::InProcess {
            ceiling: tmp.path().to_path_buf(),
        };
        (Arc::new(Scanner::new(config, lock, run)), tmp)
    }

    fn write_roots(config: &Path, roots: &[&Path]) {
        let mut cfg = crate::config::test_fixture();
        cfg.knowhow_scan_roots = roots.iter().map(|p| p.display().to_string()).collect();
        crate::config::save(&config.to_path_buf(), &cfg).unwrap();
    }

    async fn answer(rx: &mut mpsc::Receiver<ClientMsg>) -> ClientMsg {
        tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("an answer in time")
            .expect("the channel open")
    }

    fn checkout_at(dir: &Path) {
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    /// The roots come from the config FILE at each scan: an owner's edit
    /// takes the next scan, and none listed answers `no_roots`.
    #[tokio::test]
    async fn each_scan_reads_its_roots_from_the_file() {
        let root = tempfile::tempdir().unwrap();
        checkout_at(&root.path().join("api"));
        let (s, tmp) = scanner(&[]);
        let (tx, mut rx) = mpsc::channel(4);

        handle_scan(Some(&s), ObjectId::new(), true, tx.clone());
        let (outcome, _, _, findings) = parts(&answer(&mut rx).await);
        assert_eq!(outcome, ScanOutcome::NoRoots);
        assert!(findings.is_empty());

        write_roots(&tmp.path().join("config.toml"), &[root.path()]);
        handle_scan(Some(&s), ObjectId::new(), true, tx);
        let (outcome, detail, truncated, findings) = parts(&answer(&mut rx).await);
        assert_eq!(outcome, ScanOutcome::Done, "{detail:?}");
        assert!(!truncated);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0]["branch"], "main");
    }

    /// A scan delivered again is answered from what it found, never run
    /// again; a new scan reads afresh.
    #[tokio::test]
    async fn a_scan_id_runs_once() {
        let root = tempfile::tempdir().unwrap();
        checkout_at(&root.path().join("api"));
        let (s, _tmp) = scanner(&[root.path()]);
        let (tx, mut rx) = mpsc::channel(4);
        let id = ObjectId::new();

        handle_scan(Some(&s), id, true, tx.clone());
        assert_eq!(parts(&answer(&mut rx).await).3.len(), 1);
        checkout_at(&root.path().join("web"));
        handle_scan(Some(&s), id, true, tx.clone());
        assert_eq!(parts(&answer(&mut rx).await).3.len(), 1, "the first answer");
        handle_scan(Some(&s), ObjectId::new(), true, tx);
        assert_eq!(parts(&answer(&mut rx).await).3.len(), 2, "a new scan");
    }

    /// Decision 43, at the device: a secondary enrollment's frame is
    /// answered `failed`, never run; a daemon with no config says so.
    #[tokio::test]
    async fn a_secondary_enrollment_never_scans() {
        let root = tempfile::tempdir().unwrap();
        checkout_at(&root.path().join("api"));
        let (s, _tmp) = scanner(&[root.path()]);
        let (tx, mut rx) = mpsc::channel(4);

        handle_scan(Some(&s), ObjectId::new(), false, tx.clone());
        let (outcome, detail, _, findings) = parts(&answer(&mut rx).await);
        assert_eq!(outcome, ScanOutcome::Failed);
        assert!(findings.is_empty());
        assert!(detail.unwrap().contains("primary organization"));

        handle_scan(None, ObjectId::new(), true, tx);
        let (outcome, detail, _, _) = parts(&answer(&mut rx).await);
        assert_eq!(outcome, ScanOutcome::Failed);
        assert!(detail.unwrap().contains("no config file"));
    }

    /// A root another account owns is not this daemon's to read: unread,
    /// named in the detail, and the answer truncated.
    #[tokio::test]
    async fn a_root_this_daemon_cannot_read_as_its_owner_is_reported_unread() {
        let root = tempfile::tempdir().unwrap();
        checkout_at(&root.path().join("api"));
        // SAFETY: reads the caller's own credentials; it cannot fail.
        let as_root = unsafe { libc::geteuid() } == 0;
        let missing = root.path().join("missing");
        let (s, _tmp) = scanner(&[root.path(), &missing, Path::new("/")]);
        let (tx, mut rx) = mpsc::channel(4);

        handle_scan(Some(&s), ObjectId::new(), true, tx);
        let (outcome, detail, truncated, findings) = parts(&answer(&mut rx).await);
        let detail = detail.unwrap();
        if as_root {
            // Every temp folder is root's here, so none is read.
            assert_eq!(outcome, ScanOutcome::Failed, "{detail}");
            return;
        }
        assert_eq!(outcome, ScanOutcome::Done, "{detail}");
        assert!(truncated);
        assert_eq!(findings.len(), 1);
        assert!(detail.starts_with("2 of 3 roots not read"), "{detail}");
        assert!(detail.contains("missing"), "{detail}");
        assert!(detail.contains("/ (owned by root"), "{detail}");
    }

    /// A child that fails or hangs leaves its root unread.
    #[tokio::test(start_paused = true)]
    async fn a_failing_or_hanging_read_leaves_its_root_unread() {
        let root = tempfile::tempdir().unwrap();
        for (run, why) in [
            (Runner::Failing("the child said no"), "the child said no"),
            (Runner::Hangs, "ended at its deadline"),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let config = tmp.path().join("config.toml");
            write_roots(&config, &[root.path()]);
            let lock: crate::config::WriteLock = Arc::new(tokio::sync::Mutex::new(()));
            let s = Arc::new(Scanner::new(config, lock, run));
            let msg = s.scan(ObjectId::new()).await;
            let (outcome, detail, _, _) = parts(&msg);
            // SAFETY: reads the caller's own credentials; it cannot fail.
            if unsafe { libc::geteuid() } == 0 {
                continue;
            }
            assert_eq!(outcome, ScanOutcome::Failed);
            assert!(detail.unwrap().contains(why));
        }
    }

    /// The child's own read: the checkouts under its root, and a root that is
    /// no longer its account's refused.
    #[test]
    fn the_child_reads_its_root_as_its_own_account() {
        let root = tempfile::tempdir().unwrap();
        checkout_at(&root.path().join("api"));
        let req = Request::new(root.path().to_path_buf(), Duration::from_secs(30));
        // SAFETY: reads the caller's own credentials; it cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let found = produce(&req).unwrap();
        assert_eq!(found.findings.len(), 1);
        assert!(!found.cut);
        let not_mine = Request::new(PathBuf::from("/"), Duration::from_secs(30));
        assert!(
            produce(&not_mine)
                .unwrap_err()
                .contains("no longer a folder this account owns")
        );
    }

    /// A root the account cannot list is refused, never read as empty: a
    /// whole scan of nothing would archive everything earlier scans found.
    #[test]
    fn a_root_it_cannot_list_is_refused_never_read_as_empty() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: reads the caller's own credentials; it cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            // Root lists any folder, whatever its mode.
            return;
        }
        let root = tempfile::tempdir().unwrap();
        checkout_at(&root.path().join("api"));
        // Searchable, not listable: as macOS's privacy protections leave a
        // folder for a process without Full Disk Access.
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o300)).unwrap();
        let req = Request::new(root.path().to_path_buf(), Duration::from_secs(30));
        let read = produce(&req);
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let why = read.unwrap_err();
        assert!(why.contains("ermission denied"), "{why}");
    }
}
