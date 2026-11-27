// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-145 P2 — a role process's `service-logs`, by handle. Each case runs
//! alone in a child of this test binary: the stand-in `%ProgramData%` and the
//! folder held are the process's. A case that writes below the sealed folder
//! takes an elevated token, as SYSTEM writes there, so CI runs this module in
//! its elevated lane (`ci.yml`), where a runner that is not elevated fails
//! instead of skipping.

use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::test_fixture::{CHECKED, Case, child, parent, run_in_child, swap_for_junction};
use super::{HeldRollingWriter, held_dir, open_writer, switch_note, take_early_lines};
use crate::logging::{ServiceLogRole, set_service_logging};
use crate::machine_tree::Seal;
use crate::machine_tree::test_support::{aces, icacls, junction, me, protected, sddl};

const DAY: u64 = 86_400;
/// 2026-11-27, 00:00 UTC.
const DAY_1: u64 = 20_784 * DAY;
/// Older than the prune keeps: 2020-09-13.
const LONG_AGO: u64 = 1_600_000_000;

/// The case `case` of this module, in a child.
fn elevated(case: &str) {
    let root = tempfile::tempdir().unwrap();
    parent(module_path!(), case, root.path());
}

/// The clock of a case's [`HeldRollingWriter`].
static NOW: AtomicU64 = AtomicU64::new(DAY_1);

fn now() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(NOW.load(Ordering::SeqCst))
}

/// The daemon's log of day `n`, from [`DAY_1`].
fn day(n: u64) -> String {
    format!(
        "roomlerd.log.{}",
        crate::logging::format_utc_date(DAY_1 + (n - 1) * DAY)
    )
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// `Seal::Readable` on a file, as the tray reads it: protected, SYSTEM and
/// Administrators full control, Users read.
fn readable(path: &Path) -> bool {
    let sddl = sddl(path);
    let mut got = aces(&sddl);
    got.sort();
    protected(&sddl) && got == ["A;;0x1200a9;;;BU", "A;;FA;;;BA", "A;;FA;;;SY"]
}

/// AC3 — the next day's log is opened below the held folder, never by its
/// path. (a) A hard link at the next day's name to a file outside: a file of
/// one name is made, and the file outside is unchanged. (b) The folder
/// swapped for a junction: the next day lands in a sealed `service-logs` made
/// again, and nothing where the junction leads. The injected clock turns the
/// day, and every day's file is the tray's to read (`Seal::Readable`). At its
/// seam the writer opens each day's file by path, as tracing-appender does.
#[test]
fn the_next_days_log_is_opened_below_the_held_folder() {
    let Some(Case {
        root,
        dir,
        elsewhere,
    }) = child(true)
    else {
        return elevated("the_next_days_log_is_opened_below_the_held_folder");
    };
    let logs = held_dir(&dir).expect("held");
    let mut log = HeldRollingWriter::open(logs, "roomlerd.log", now).unwrap();
    log.write_all(b"day one\n").unwrap();

    let outside = root.join("outside.txt");
    std::fs::write(&outside, b"theirs\n").unwrap();
    std::fs::hard_link(&outside, dir.join(day(2))).unwrap();
    NOW.store(DAY_1 + DAY, Ordering::SeqCst);
    log.write_all(b"day two\n").unwrap();
    assert_eq!(
        std::fs::read(&outside).unwrap(),
        b"theirs\n",
        "day two was written into a hard link to a file outside"
    );
    assert_eq!(std::fs::read(dir.join(day(2))).unwrap(), b"day two\n");
    for name in [day(1), day(2)] {
        let file = dir.join(name);
        assert!(readable(&file), "{}: {}", file.display(), sddl(&file));
    }

    swap_for_junction(&dir, &elsewhere);
    NOW.store(DAY_1 + 2 * DAY, Ordering::SeqCst);
    log.write_all(b"day three\n").unwrap();
    assert!(
        names_in(&elsewhere).is_empty(),
        "day three was opened where the junction leads: {:?}",
        names_in(&elsewhere)
    );
    assert!(dir.is_dir() && !is_link(&dir), "service-logs made again");
    let file = dir.join(day(3));
    assert_eq!(std::fs::read(&file).unwrap(), b"day three\n");
    assert!(readable(&file), "{}", sddl(&file));
    println!("{CHECKED}");
}

/// AC3 — the prune lists the held folder and deletes by handle, by the same
/// name and age rule: an old log there goes, a new one and a name not ours
/// stay; with the folder swapped for a junction, an old log where it leads
/// stays. At its seam the prune reads and deletes by path.
#[test]
fn the_prune_goes_by_handle_never_where_a_junction_leads() {
    let Some(Case { dir, elsewhere, .. }) = child(true) else {
        return elevated("the_prune_goes_by_handle_never_where_a_junction_leads");
    };
    let old = |path: &Path| {
        std::fs::write(path, b"x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(LONG_AGO))
            .unwrap();
    };
    old(&dir.join("roomlerd.log.2020-09-13"));
    old(&dir.join("notes.txt"));
    std::fs::write(dir.join(day(1)), b"x").unwrap();
    crate::logging::prune_old_logs(&dir, 14);
    assert_eq!(names_in(&dir), ["notes.txt".to_string(), day(1)]);

    swap_for_junction(&dir, &elsewhere);
    old(&elsewhere.join("roomlerd.log.2020-09-13"));
    crate::logging::prune_old_logs(&dir, 14);
    assert_eq!(
        names_in(&elsewhere),
        ["roomlerd.log.2020-09-13"],
        "the prune deleted where the junction leads"
    );
    println!("{CHECKED}");
}

/// AC3 — the panic file is written below the held folder by handle: with the
/// folder swapped for a junction, it lands in a sealed `service-logs` made
/// again, the tray's to read, and nothing where the junction leads. At its
/// seam the hook writes it by path.
#[test]
fn a_panic_file_is_written_below_the_held_folder() {
    let Some(Case { dir, elsewhere, .. }) = child(true) else {
        return elevated("a_panic_file_is_written_below_the_held_folder");
    };
    crate::logging::install_panic_hook(dir.clone());
    swap_for_junction(&dir, &elsewhere);
    let _ = std::panic::catch_unwind(|| panic!("a panic for its file"));
    assert!(
        names_in(&elsewhere).is_empty(),
        "the panic file was written where the junction leads: {:?}",
        names_in(&elsewhere)
    );
    let panics: Vec<String> = names_in(&dir)
        .into_iter()
        .filter(|n| n.starts_with("panic-"))
        .collect();
    assert_eq!(panics.len(), 1, "{:?}", names_in(&dir));
    let file = dir.join(&panics[0]);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("a panic for its file"), "{text}");
    assert!(readable(&file), "{}", sddl(&file));
    println!("{CHECKED}");
}

/// AC3 — stderr goes to the day's log by handle (#3292, #3438): with the
/// folder swapped for a junction, the next day's file is opened in a sealed
/// `service-logs` made again, and nothing where the junction leads. Its
/// stderr is the file, so the case reports on stdout. At its seam stderr's
/// file is opened by path.
#[test]
fn stderr_follows_the_day_below_the_held_folder() {
    let Some(Case { dir, elsewhere, .. }) = child(true) else {
        return elevated("stderr_follows_the_day_below_the_held_folder");
    };
    let check = |ok: bool, what: &str| {
        if !ok {
            println!("FAILED: {what}");
            std::process::exit(1);
        }
    };
    use crate::logging::stderr_file::repoint;
    check(repoint(&dir, "2026-11-27"), "on day one's file");
    eprintln!("stderr on day one");
    swap_for_junction(&dir, &elsewhere);
    check(repoint(&dir, "2026-11-28"), "on day two's file");
    eprintln!("stderr on day two");
    let leaked = names_in(&elsewhere);
    check(
        leaked.is_empty(),
        &format!("stderr's file was opened where the junction leads: {leaked:?}"),
    );
    let file = dir.join("roomlerd.log.2026-11-28");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    check(
        text.contains("stderr on day two"),
        &format!("day two: {text:?}"),
    );
    check(readable(&file), &sddl(&file));
    println!("{CHECKED}");
}

/// A refusal costs neither the log nor the start: with the machine folder's
/// parent a junction, the hold is refused and says why, nothing is made where
/// the junction leads and nothing is held; the start then logs in its
/// fallback, with that reason in its note (#3073). At its seam the folder is
/// made by path, through the junction. No elevated token is needed.
#[test]
fn a_refused_hold_falls_back_and_says_why() {
    let Some(Case {
        root,
        dir,
        elsewhere,
    }) = child(false)
    else {
        let root = tempfile::tempdir().unwrap();
        let test = super::test_fixture::test_path(
            module_path!(),
            "a_refused_hold_falls_back_and_says_why",
        );
        return run_in_child(root.path(), &test, &[]);
    };
    let roomler = root.join("ProgramData").join("roomler");
    // The stand-in gives an account no right to delete what it made there;
    // as the folder's owner, an account that is not elevated grants itself one.
    assert!(
        icacls(&roomler, &["/grant", &format!("*{}:(D)", me())]),
        "DELETE on {}",
        roomler.display()
    );
    std::fs::rename(&roomler, root.join("roomler.old")).unwrap();
    junction(&roomler, &elsewhere);
    set_service_logging(ServiceLogRole::Worker);
    let refused = match open_writer(&dir, "roomlerd.log") {
        Err(e) => e.to_string(),
        Ok(writer) => panic!("opened through a junction: {}", writer.is_some()),
    };
    assert!(refused.contains("link"), "{refused}");
    assert!(
        names_in(&elsewhere).is_empty(),
        "made where the junction leads: {:?}",
        names_in(&elsewhere)
    );
    assert!(held_dir(&dir).is_none(), "nothing held");

    let fallback = root.join("profile-logs");
    let (opened, notes) =
        crate::logging::open_log_appender(Some(dir.clone()), Some(fallback.clone()), |d| {
            open_writer(d, "roomlerd.log").map(|held| held.is_some())
        });
    assert_eq!(opened, Some((fallback, false)), "the fallback, by path");
    assert!(
        notes.len() == 1 && notes[0].contains("link") && notes[0].contains("instead"),
        "{notes:?}"
    );
    println!("{CHECKED}");
}

/// Only a role process holds its log folder, and only with the switch on. No
/// role: today's writer, and no note. The switch at `0`: today's writer, and
/// today's writers for every other file (nothing held), with one WARN that
/// names the switch. No elevated token is needed.
#[test]
fn only_a_role_process_with_the_switch_on_holds_its_log_folder() {
    let Some(Case { dir, .. }) = child(false) else {
        let root = tempfile::tempdir().unwrap();
        let test = super::test_fixture::test_path(
            module_path!(),
            "only_a_role_process_with_the_switch_on_holds_its_log_folder",
        );
        return run_in_child(root.path(), &test, &[("ROOMLERD_MACHINE_TREE_HELD", "0")]);
    };
    assert!(
        open_writer(&dir, "roomlerd.log").unwrap().is_none(),
        "no role"
    );
    assert!(switch_note().is_none(), "no role reads the switch");
    set_service_logging(ServiceLogRole::Host);
    let writer = open_writer(&dir, ServiceLogRole::Host.basename()).unwrap();
    assert!(writer.is_none(), "the switch at 0");
    assert!(held_dir(&dir).is_none(), "nothing held");
    assert!(
        switch_note().is_some_and(|n| n.contains("ROOMLERD_MACHINE_TREE_HELD=0")),
        "{:?}",
        switch_note()
    );
    println!("{CHECKED}");
}

/// A reader that does not share deletion (PowerShell's `Get-Content`, `type`)
/// still opens the day's log while the writer holds it, as it did
/// tracing-appender's: no handle a writer keeps open holds `DELETE`, and
/// stderr's two come from the same `append_file`. The control: a handle that
/// holds it, as `HeldPath::append_file`'s own does, shuts such a reader out.
#[test]
fn a_reader_that_does_not_share_deletion_reads_the_days_log() {
    let Some(Case { dir, .. }) = child(true) else {
        return elevated("a_reader_that_does_not_share_deletion_reads_the_days_log");
    };
    use std::io::Read as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    let read = || {
        let mut text = String::new();
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(dir.join(day(1)))
            .and_then(|mut file| file.read_to_string(&mut text))
            .map(|_| text)
    };
    let logs = held_dir(&dir).expect("held");
    let mut log = HeldRollingWriter::open(logs, "roomlerd.log", now).unwrap();
    log.write_all(b"a line\n").unwrap();

    let with_delete = logs.lock().append_file(&day(1), Seal::Readable).unwrap();
    assert!(read().is_err(), "the control: a handle with DELETE open");
    drop(with_delete);
    assert_eq!(read().unwrap(), "a line\n", "with the writer's handle open");
    println!("{CHECKED}");
}

/// The leaf policy's one WARN reaches the log: a start holds `service-logs`
/// before its subscriber exists, so what the hold logs is kept and said once
/// it does, each line once. With a junction where `service-logs` is, the hold
/// moves the junction aside and makes a sealed folder, nothing made where it
/// leads.
#[test]
fn what_the_hold_moved_aside_is_said_once_the_log_is_up() {
    let Some(Case { dir, elsewhere, .. }) = child(false) else {
        return elevated("what_the_hold_moved_aside_is_said_once_the_log_is_up");
    };
    junction(&dir, &elsewhere);
    set_service_logging(ServiceLogRole::Worker);
    let writer = open_writer(&dir, "roomlerd.log").unwrap();
    assert!(writer.is_some(), "held");
    assert!(dir.is_dir() && !is_link(&dir), "a sealed service-logs made");
    assert!(
        names_in(&elsewhere).is_empty(),
        "{:?}",
        names_in(&elsewhere)
    );
    let lines = take_early_lines();
    assert!(
        lines.len() == 1 && lines[0].contains("is a link") && lines[0].contains("moved aside"),
        "{lines:?}"
    );
    assert!(take_early_lines().is_empty(), "each line once");
    println!("{CHECKED}");
}
