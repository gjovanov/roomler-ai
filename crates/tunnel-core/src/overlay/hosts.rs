// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD

//! FR-72 P6 — the last rung of the MagicDNS ladder.
//!
//! The preferred path is the OS split-DNS steer (`dns::configure_os`): dynamic,
//! whole-zone, nothing written to disk. On some managed hosts it cannot work at
//! all — a corporate DNS-enforcement layer intercepts the machine's own DNS
//! egress and refuses queries to any non-approved resolver, **including the
//! host's own overlay address**. Field-measured: the same resolver, at the same
//! moment, answers a peer correctly and refuses its own host with `REFUSED`
//! (RA=0, which our resolver never emits — `dns.rs` always sets RA=1).
//!
//! The hosts file is not on the DNS path at all, which is exactly why it
//! survives that. This module owns a **delimited block** in it, so the file's
//! other lines — a corporate VPN's, an operator's — are preserved byte for byte.
//!
//! ⚠️ **FQDNs only, never bare labels.** Writing `mars` into a corporate
//! machine's hosts file would shadow a real internal host; only
//! `mars.<magic domain>` is ever written.
//!
//! ⚠️ **A stale entry is the hazard**, not the write. Overlay addresses are
//! recycled, so a line left behind by a hard exit can silently send traffic to
//! a different machine — the same class of bug as pooling an overlay address
//! before its tombstone. The block is therefore cleared when the ladder climbs
//! back to DNS, cleared on runtime teardown, and cleared at runtime start
//! before anything else (the boot reconciler: whatever a previous process left
//! is not trusted, it is removed and re-derived).

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;

/// Markers delimiting the block this module owns. Anything between them is
/// ours to rewrite; anything outside is never touched.
const BEGIN: &str = "# BEGIN roomler magicdns (managed — edits here are overwritten)";
const END: &str = "# END roomler magicdns";

/// One name's addresses. `v6` is `None` when AAAA answers are switched off.
pub type Entry = (String, Ipv4Addr, Option<Ipv6Addr>);

/// The OS hosts file.
pub fn hosts_path() -> PathBuf {
    #[cfg(windows)]
    {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        PathBuf::from(root).join(r"System32\drivers\etc\hosts")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/etc/hosts")
    }
}

/// Render the managed block for `entries` (already sorted by the caller).
fn render(entries: &[Entry], nl: &str) -> String {
    let mut out = String::new();
    out.push_str(BEGIN);
    out.push_str(nl);
    for (name, v4, v6) in entries {
        out.push_str(&format!("{v4} {name}{nl}"));
        if let Some(v6) = v6 {
            out.push_str(&format!("{v6} {name}{nl}"));
        }
    }
    out.push_str(END);
    out
}

/// Strip our block from `content`, returning the remainder. A file with no
/// block comes back unchanged; an unterminated block (a previous process killed
/// mid-write) is dropped to the end, which is the safe reading — a half-written
/// block must never be left behind as live entries.
fn strip_block(content: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in content.lines() {
        let t = line.trim();
        if t == BEGIN {
            inside = true;
            continue;
        }
        if t == END {
            inside = false;
            continue;
        }
        if !inside {
            out.push(line);
        }
    }
    out.join("\n")
}

/// Replace the managed block with `entries`; an empty slice removes it.
///
/// Returns `Ok(true)` when the file was actually rewritten — the caller uses
/// that to keep a periodic reconcile from touching a system file every tick.
pub fn write_block(entries: &[Entry]) -> io::Result<bool> {
    let path = hosts_path();
    // A missing hosts file is not an error to us: treat it as empty and create
    // it. (It is always present on Windows, and effectively always on Unix.)
    let original = std::fs::read_to_string(&path).unwrap_or_default();

    // Match the file's own line endings rather than the platform's: a Windows
    // hosts file is CRLF, but an operator's editor may have left LF, and
    // rewriting the whole file in the other convention would show up as a
    // wholesale diff in any tool watching it.
    let nl = if original.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };

    let body = strip_block(&original);
    let body = body.trim_end_matches(['\r', '\n']).to_string();

    let mut next = if nl == "\r\n" {
        body.replace('\n', "\r\n")
    } else {
        body
    };
    if !entries.is_empty() {
        if !next.is_empty() {
            next.push_str(nl);
        }
        next.push_str(&render(entries, nl));
    }
    if !next.is_empty() {
        next.push_str(nl);
    }

    if next == original {
        return Ok(false);
    }
    write_atomic(&path, &next)?;
    Ok(true)
}

/// Remove the managed block entirely.
pub fn clear_block() -> io::Result<bool> {
    write_block(&[])
}

/// Write via a sibling temp file + rename, so a crash mid-write cannot leave
/// the system's hosts file truncated.
///
/// ⚠️ Falls back to a direct write when the rename fails: on Windows an AV or
/// backup agent can hold a transient handle on `etc\hosts`, and refusing to
/// write at all would mean the fallback silently stops working on exactly the
/// managed hosts it exists for. The direct write is the lesser risk — the file
/// is small and written in one call.
fn write_atomic(path: &std::path::Path, content: &str) -> io::Result<()> {
    let tmp = path.with_extension("roomler-tmp");
    match std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            tracing::debug!(%e, "magicdns hosts: atomic replace failed, writing in place");
            std::fs::write(path, content)
        }
    }
}

/// What one tick does to the hosts file, decided apart from the file.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// Leave the file alone.
    Hold,
    /// Write the block, or bring it in step with the netmap.
    Write,
    /// Remove the block, if there is one.
    Clear(&'static str),
}

/// #1363 — the ladder's decision for one tick. Each NEW reading (by its
/// `seq`) moves the rung once; between readings an active block is only kept
/// in step with the netmap, so a peer that joined or changed address is never
/// missing from it. Any answer is the DNS rung, this node's resolver or not:
/// names resolve, which is all the rung is for (the status is stricter).
fn step(enabled: bool, active: bool, seen: &mut u64, reading: &super::dns_probe::Reading) -> Step {
    if !enabled {
        return Step::Clear("switch off");
    }
    if reading.seq == 0 || reading.seq == *seen {
        return if active { Step::Write } else { Step::Hold };
    }
    *seen = reading.seq;
    if reading.outcome.dns_resolves() {
        // Never ratchet: DNS works, so give the file back.
        Step::Clear("the OS DNS path reaches the resolver again")
    } else {
        Step::Write
    }
}

/// The MagicDNS resolution ladder: prefer the OS DNS steer, fall back to the
/// hosts file, and **climb back** as soon as DNS works again.
///
/// The rung is chosen by measurement, never assumed — the same rule the carrier
/// cascade follows. The measurement is [`dns::PROBE_LABEL`], a name only an
/// overlay resolver answers, resolved through the OS: it succeeds exactly when
/// the OS DNS path reaches one.
///
/// #1363 — the lookup is no longer the ladder's own: one probe task
/// ([`super::dns_probe::OsProbe`]) makes it once a minute, off the runtime
/// loop, and the status reads the same reading. The ladder acts on each new
/// reading once ([`step`]).
///
/// [`dns::PROBE_LABEL`]: super::dns::PROBE_LABEL
pub struct Ladder {
    enabled: bool,
    magic_domain: String,
    /// Is our block currently in the file?
    active: bool,
    /// The last probe reading acted on ([`super::dns_probe::Reading::seq`]).
    seen: u64,
}

impl Ladder {
    /// `enabled` is the kill switch (`ROOMLERD_MAGICDNS_HOSTS`).
    ///
    /// ⚠️ Construction CLEARS any existing block — the boot reconciler. A block
    /// left by a previous process describes a netmap we have not verified, and
    /// overlay addresses are recycled, so it is removed and re-derived rather
    /// than trusted.
    pub fn new(magic_domain: String, enabled: bool) -> Self {
        match clear_block() {
            Ok(true) => tracing::info!("magicdns hosts: cleared a block left by a previous run"),
            Ok(false) => {}
            Err(e) => tracing::warn!(%e, "magicdns hosts: could not clear a stale block"),
        }
        Self {
            enabled,
            magic_domain,
            active: false,
            seen: 0,
        }
    }

    /// The kill switch, as constructed: the runtime runs the probe for the
    /// ladder while it is on, whatever else the host's state.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Is the hosts-file block serving the names right now? (#1363: the
    /// status's `names served from the hosts file`.)
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Called from the runtime's periodic tick with the probe's latest
    /// reading. Never waits on the network: the lookup is the probe task's.
    pub fn tick(
        &mut self,
        reading: &super::dns_probe::Reading,
        names: &super::dns::NameMap,
        answer_aaaa: bool,
    ) {
        match step(self.enabled, self.active, &mut self.seen, reading) {
            Step::Hold => {}
            Step::Clear(why) => self.deactivate(why),
            Step::Write => {
                if !self.active {
                    tracing::warn!(
                        domain = %self.magic_domain,
                        "magicdns: the OS DNS path does not reach our resolver — \
                         falling back to hosts-file entries"
                    );
                }
                self.active = true;
                self.write(names, answer_aaaa);
            }
        }
    }

    fn write(&mut self, names: &super::dns::NameMap, answer_aaaa: bool) {
        // `try_read` on purpose: the map is written by the netmap path, and a
        // periodic reconcile must never block it. A skipped tick costs at most
        // one interval.
        let Ok(map) = names.try_read() else { return };
        let mut entries: Vec<Entry> = map
            .iter()
            .map(|(label, v4)| {
                let name = format!("{label}.{}", self.magic_domain);
                let v6 = answer_aaaa.then(|| super::router::derive_overlay_v6(*v4));
                (name, *v4, v6)
            })
            .collect();
        drop(map);
        // Deterministic order, so an unchanged netmap never rewrites the file.
        entries.sort();
        match write_block(&entries) {
            Ok(true) => tracing::info!(
                count = entries.len(),
                "magicdns hosts: wrote the managed block"
            ),
            Ok(false) => {}
            Err(e) => tracing::warn!(%e, "magicdns hosts: write failed"),
        }
    }

    fn deactivate(&mut self, why: &str) {
        if !self.active {
            return;
        }
        self.active = false;
        match clear_block() {
            Ok(_) => tracing::info!(%why, "magicdns hosts: removed the managed block"),
            Err(e) => tracing::warn!(%e, %why, "magicdns hosts: could not remove the block"),
        }
    }

    /// Runtime teardown. ⚠️ Must run on every exit path: a block that outlives
    /// the daemon points names at addresses nothing is serving, and a recycled
    /// overlay address makes that actively wrong rather than merely dead.
    pub fn shutdown(&mut self) {
        self.deactivate("runtime shutting down");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, v4: [u8; 4]) -> Entry {
        (name.into(), Ipv4Addr::from(v4), None)
    }

    /// #1363 — the ladder reads the shared probe's readings and decides as it
    /// did when the lookup was its own: each new reading moves the rung once,
    /// any answer is the DNS rung (this node's resolver or another), and an
    /// active block follows the netmap between readings.
    #[test]
    fn the_ladder_moves_once_a_reading_and_takes_any_answer_as_dns() {
        use super::super::dns_probe::{Outcome, Reading};
        let r = |seq, outcome| Reading {
            seq,
            outcome,
            at: None,
            reason: None,
            next_at: None,
        };
        let other = std::net::IpAddr::V4(Ipv4Addr::new(100, 65, 4, 2));
        let mut seen = 0;

        // Nothing measured yet: nothing to do.
        assert_eq!(
            step(true, false, &mut seen, &Reading::default()),
            Step::Hold
        );
        // Unanswered: the hosts rung.
        assert_eq!(
            step(true, false, &mut seen, &r(1, Outcome::Unanswered)),
            Step::Write
        );
        assert_eq!(seen, 1);
        // The same reading again moves nothing; the active block only follows
        // the netmap.
        assert_eq!(
            step(true, true, &mut seen, &r(1, Outcome::Unanswered)),
            Step::Write
        );
        assert_eq!(
            step(true, false, &mut seen, &r(1, Outcome::Unanswered)),
            Step::Hold
        );
        // Another node's resolver answering is DNS working: climb back.
        assert_eq!(
            step(
                true,
                true,
                &mut seen,
                &r(2, Outcome::AnsweredElsewhere(other))
            ),
            Step::Clear("the OS DNS path reaches the resolver again")
        );
        assert_eq!(
            step(true, true, &mut seen, &r(3, Outcome::Answered)),
            Step::Clear("the OS DNS path reaches the resolver again")
        );
        // The switch off clears, whatever the reading.
        assert_eq!(
            step(false, true, &mut seen, &r(4, Outcome::Unanswered)),
            Step::Clear("switch off")
        );
        assert_eq!(seen, 3, "a switched-off ladder takes no reading");
    }

    #[test]
    fn foreign_lines_survive_and_the_block_round_trips() {
        let foreign = "127.0.0.1 localhost\n10.0.0.5 corp-fileserver\n";
        let with = format!(
            "{foreign}{}\n",
            render(&[e("mars.myorg.example", [100, 65, 4, 14])], "\n")
        );
        // Stripping ours leaves theirs untouched, byte for byte.
        assert_eq!(
            strip_block(&with).trim_end_matches('\n'),
            foreign.trim_end_matches('\n')
        );
    }

    #[test]
    fn an_unterminated_block_is_dropped_not_kept() {
        // A process killed mid-write leaves BEGIN with no END. Those lines must
        // NOT survive as live entries: a stale overlay address silently routes
        // to whatever machine holds it now.
        let broken = format!("127.0.0.1 localhost\n{BEGIN}\n100.65.4.14 mars.myorg.example\n");
        let left = strip_block(&broken);
        assert!(
            !left.contains("mars.myorg.example"),
            "a half-written block must be discarded, not adopted"
        );
        assert!(left.contains("localhost"), "foreign lines still survive");
    }

    #[test]
    fn only_fqdns_are_rendered() {
        // ⚠️ A bare label would shadow a real corporate host of the same name.
        let out = render(&[e("mars.myorg.example", [100, 65, 4, 14])], "\n");
        for line in out.lines().filter(|l| !l.starts_with('#')) {
            let name = line.split_whitespace().nth(1).unwrap();
            assert!(name.contains('.'), "{name} is not an FQDN");
        }
    }

    #[test]
    fn v6_is_emitted_beside_v4_when_present() {
        let entries: Vec<Entry> = vec![(
            "mars.myorg.example".into(),
            Ipv4Addr::new(100, 65, 4, 14),
            Some("fd72:6f6f:6d6c::6441:40e".parse().unwrap()),
        )];
        let out = render(&entries, "\n");
        assert!(out.contains("100.65.4.14 mars.myorg.example"));
        assert!(out.contains("fd72:6f6f:6d6c::6441:40e mars.myorg.example"));
    }
}
