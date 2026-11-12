// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-145 T1-T10 — `HeldPath` never acts through a link. Each case carries
//! its control: the same act with no link succeeds, so a helper that refuses
//! everything fails them. Junctions (`mklink /J`) need no privilege; a case
//! that needs a symbolic link, Administrators as an owner, or another
//! account's folder starts with `elevated_or_skip`, which fails in CI's
//! elevated lane instead of skipping.

use std::path::{Path, PathBuf};

use super::{HeldPath, Seal, Why};
use crate::machine_tree::test_support::{
    between_check_and_use, elevated_or_skip, icacls, junction, local_guest, owner, sddl, sealed,
};

const CHECKED: &str = "held-path case: checked";
const CHILD: &str = "ROOMLER_NODE_TEST_HELD_CHILD";

fn base_of(dir: &Path) -> HeldPath {
    HeldPath::base(dir).expect("a test's own folder is a base")
}

fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// The names in `dir`, sorted; none when it is not there.
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

/// The one entry of `dir` whose name starts with `prefix`.
fn moved_in(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    names_in(dir)
        .into_iter()
        .filter(|n| n.starts_with(prefix))
        .map(|n| dir.join(n))
        .collect()
}

fn named(at: &Path, name: &str) -> bool {
    at.file_name().and_then(|n| n.to_str()) == Some(name)
}

/// The case `name`, in a child process of this test binary with one test
/// thread: a case that sets #3743's swap hook, which is process-wide. `true`
/// in the child, which runs the case; the parent waits for it and checks it
/// said it ran.
fn in_child(name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let module = module_path!();
    let test = format!(
        "{}::{name}",
        module.split_once("::").map_or(module, |(_, rest)| rest)
    );
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test.as_str(), "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{said}");
    assert!(said.contains(CHECKED), "the child ran the case: {said}");
    false
}

/// T1 — a junction as a middle name: `walk_sealed` is refused there, and
/// nothing is made where it leads. Control: a real folder on the way, the
/// leaf made.
#[test]
fn a_walk_refuses_a_junction_on_the_way() {
    let root = tempfile::tempdir().unwrap();
    let elsewhere = root.path().join("elsewhere");
    let base = root.path().join("base");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::create_dir(&base).unwrap();
    junction(&base.join("a"), &elsewhere);
    let refused = base_of(&base)
        .walk_sealed(&["a", "b", "crashes"], Seal::Private)
        .expect_err("a junction on the way is refused");
    assert!(matches!(refused.why, Why::Link), "{refused}");
    assert!(named(&refused.at, "a"), "{refused}");
    assert!(names_in(&elsewhere).is_empty(), "nothing made where it led");

    let control = root.path().join("control");
    std::fs::create_dir_all(control.join("a").join("b")).unwrap();
    let held = base_of(&control)
        .walk_sealed(&["a", "b", "crashes"], Seal::Private)
        .expect("a real folder on the way");
    let crashes = control.join("a").join("b").join("crashes");
    assert!(crashes.is_dir() && !is_link(&crashes), "{held:?}");
    assert!(named(held.path(), "crashes"), "{held:?}");
}

/// T2 — T1 with a directory symbolic link.
#[test]
fn a_walk_refuses_a_symbolic_link_on_the_way() {
    if !elevated_or_skip("T2: a directory symbolic link on the way") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let elsewhere = root.path().join("elsewhere");
    let base = root.path().join("base");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::create_dir(&base).unwrap();
    std::os::windows::fs::symlink_dir(&elsewhere, base.join("a")).expect("a symbolic link");
    let refused = base_of(&base)
        .walk_sealed(&["a", "b", "crashes"], Seal::Private)
        .expect_err("a symbolic link on the way is refused");
    assert!(matches!(refused.why, Why::Link), "{refused}");
    assert!(names_in(&elsewhere).is_empty(), "nothing made where it led");

    let control = root.path().join("control");
    std::fs::create_dir_all(control.join("a").join("b")).unwrap();
    base_of(&control)
        .walk_sealed(&["a", "b", "crashes"], Seal::Private)
        .expect("a real folder on the way");
    assert!(control.join("a").join("b").join("crashes").is_dir());
    println!("{CHECKED}");
}

/// T3 — once a name is held, it is renamed and a junction put at its name
/// (#3743's `between_check_and_use`): the file lands in the held folder, or
/// the walk is refused; never where the junction leads. (a) the walk's last
/// name, once held; (b) a middle name, between two halves of a walk, so the
/// second half starts below the renamed folder. (While a walk holds a folder
/// below a name, NTFS itself refuses to rename that name.)
#[test]
fn a_folder_swapped_for_a_junction_mid_walk_is_not_followed() {
    if !in_child("a_folder_swapped_for_a_junction_mid_walk_is_not_followed") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    // The swap of `dir\name`: renamed to `<name>.old`, a junction in its
    // place, to `elsewhere`.
    let swap = |dir: &Path, name: &str, elsewhere: &Path| {
        let (at, elsewhere) = (dir.join(name), elsewhere.to_path_buf());
        move || {
            std::fs::rename(&at, at.with_extension("old")).unwrap();
            junction(&at, &elsewhere);
        }
    };

    // (a) `crashes`, once the walk holds it.
    let base = root.path().join("a-case");
    let elsewhere = root.path().join("a-elsewhere");
    let b = base.join("a").join("b");
    std::fs::create_dir_all(b.join("crashes")).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    between_check_and_use(swap(&b, "crashes", &elsewhere));
    let held = base_of(&base)
        .walk(&["a", "b", "crashes"])
        .unwrap()
        .expect("held");
    held.write_file("x.json", b"{}", Seal::Private)
        .expect("written below the held folder");
    assert!(is_link(&b.join("crashes")), "the swap ran");
    assert_eq!(
        names_in(&b.join("crashes.old")),
        ["x.json"],
        "where it was held"
    );
    assert!(
        names_in(&elsewhere).is_empty(),
        "nothing where the junction leads"
    );

    // (b) `a`, between `walk(["a"])` and the rest of the way.
    let base = root.path().join("b-case");
    let elsewhere = root.path().join("b-elsewhere");
    std::fs::create_dir_all(base.join("a").join("b").join("crashes")).unwrap();
    std::fs::create_dir_all(elsewhere.join("b").join("crashes")).unwrap();
    between_check_and_use(swap(&base, "a", &elsewhere));
    let a = base_of(&base).walk(&["a"]).unwrap().expect("a held");
    assert!(is_link(&base.join("a")), "the swap ran");
    let landed = base.join("a.old").join("b").join("crashes");
    match a.walk_sealed(&["b", "crashes"], Seal::Private) {
        Ok(held) => {
            held.write_file("y.json", b"{}", Seal::Private).unwrap();
            assert_eq!(names_in(&landed), ["y.json"], "{held:?}");
        }
        Err(refused) => assert!(
            matches!(refused.why, Why::Elsewhere { .. }),
            "refused because the held folder moved: {refused}"
        ),
    }
    assert!(
        names_in(&elsewhere.join("b").join("crashes")).is_empty(),
        "nothing where the junction leads"
    );
    println!("{CHECKED}");
}

/// T4 — a base reached through a junction is refused, naming its final path.
/// Control: the same base named by its 8.3 short name is accepted, as
/// GitHub's runners name `TEMP` (skipped, with a note, on a volume without
/// short names).
#[test]
fn a_base_reached_through_a_link_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir_all(real.join("base")).unwrap();
    junction(&root.path().join("j"), &real);
    let refused = HeldPath::base(&root.path().join("j").join("base"))
        .expect_err("a base through a junction is refused");
    match &refused.why {
        Why::Elsewhere { final_path } => {
            assert!(
                final_path.ends_with(Path::new("real").join("base")),
                "{refused}"
            )
        }
        other => panic!("refused as elsewhere, not {other:?}"),
    }

    let long = root.path().join("a folder with a long name");
    std::fs::create_dir(&long).unwrap();
    let short = short_path(&long);
    if short == long {
        eprintln!(
            "NOTE T4's control: this volume makes no 8.3 names; the short-name case is not exercised here"
        );
    }
    std::fs::create_dir(long.join("below")).unwrap();
    let held = HeldPath::base(&short).expect("a short name of a real folder is accepted");
    assert_eq!(held.path(), short, "named as the caller named it");
    let below = held.open_dir("below").unwrap().expect("a walk below it");
    assert!(named(below.path(), "below"), "{below:?}");
}

fn short_path(path: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: a NUL-terminated path; `buf` holds its length in units.
    let n = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(
            wide.as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    } as usize;
    assert!(n > 0 && n < buf.len(), "GetShortPathNameW");
    PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]))
}

/// T5 — at the name a write publishes: (a) a file symbolic link, (b) a
/// junction, (c) a hard link. (a) and (c) are replaced as names, and what
/// they led to is byte-identical; (b) is refused, nothing left behind.
/// Control: a plain file is replaced.
#[test]
fn a_write_replaces_the_name_and_never_writes_through_it() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("dir");
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&dir).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    let held = base_of(&dir);

    // Control: a plain file.
    std::fs::write(dir.join("rec.json"), b"old").unwrap();
    held.write_file("rec.json", b"new", Seal::Private).unwrap();
    assert_eq!(std::fs::read(dir.join("rec.json")).unwrap(), b"new");

    // (c) A hard link.
    std::fs::remove_file(dir.join("rec.json")).unwrap();
    let outside2 = root.path().join("outside2.txt");
    std::fs::write(&outside2, b"theirs").unwrap();
    std::fs::hard_link(&outside2, dir.join("rec.json")).unwrap();
    held.write_file("rec.json", b"new", Seal::Private).unwrap();
    assert_eq!(std::fs::read(&outside2).unwrap(), b"theirs");
    assert_eq!(std::fs::read(dir.join("rec.json")).unwrap(), b"new");
    let one = HeldPath::base(&dir)
        .unwrap()
        .read_file("rec.json", 64)
        .expect("a file of one name");
    assert_eq!(one.as_deref(), Some(&b"new"[..]));

    // (b) A junction.
    std::fs::remove_file(dir.join("rec.json")).unwrap();
    junction(&dir.join("rec.json"), &elsewhere);
    let refused = held
        .write_file("rec.json", b"new", Seal::Private)
        .expect_err("a junction at the name is refused");
    assert!(names_in(&elsewhere).is_empty(), "{refused}");
    assert_eq!(names_in(&dir), ["rec.json"], "no temp file left: {refused}");
    assert!(is_link(&dir.join("rec.json")));

    // (a) A file symbolic link.
    if elevated_or_skip("T5 (a): a file symbolic link at the name") {
        std::fs::remove_dir(dir.join("rec.json")).unwrap();
        let outside = root.path().join("outside.txt");
        std::fs::write(&outside, b"theirs").unwrap();
        std::os::windows::fs::symlink_file(&outside, dir.join("rec.json")).unwrap();
        held.write_file("rec.json", b"new", Seal::Private).unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"theirs");
        assert!(!is_link(&dir.join("rec.json")));
        assert_eq!(std::fs::read(dir.join("rec.json")).unwrap(), b"new");
        println!("{CHECKED}: T5 (a)");
    }
}

/// T6 — a log name that is a hard link to another file: the line goes to a
/// new file of one name, and the other file is unchanged. Control: a name
/// that is not there is made, and the line written.
#[test]
fn an_append_never_writes_into_a_hard_link() {
    use std::io::Write as _;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("logs");
    std::fs::create_dir(&dir).unwrap();
    let outside = root.path().join("outside.txt");
    std::fs::write(&outside, b"theirs\n").unwrap();
    let log = dir.join("roomlerd.log.2026-11-12");
    std::fs::hard_link(&outside, &log).unwrap();
    let held = base_of(&dir);
    let mut file = held
        .append_file("roomlerd.log.2026-11-12", Seal::Readable)
        .expect("a fresh log file");
    file.write_all(b"a line\n").unwrap();
    drop(file);
    assert_eq!(std::fs::read(&outside).unwrap(), b"theirs\n");
    assert_eq!(std::fs::read(&log).unwrap(), b"a line\n");

    let mut file = held
        .append_file("roomlerd.log.2026-11-13", Seal::Readable)
        .expect("made");
    file.write_all(b"a line\n").unwrap();
    drop(file);
    assert_eq!(
        std::fs::read(dir.join("roomlerd.log.2026-11-13")).unwrap(),
        b"a line\n"
    );
}

/// T7 — `crashes` is a junction: a real, sealed `crashes` is made, the
/// junction kept beside it as `crashes.moved-<unix>`, nothing where it led.
#[test]
fn a_junction_where_the_leaf_goes_is_moved_aside() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("machine");
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&dir).unwrap();
    std::fs::create_dir(&elsewhere).unwrap();
    junction(&dir.join("crashes"), &elsewhere);
    let held = base_of(&dir)
        .sealed_dir("crashes", Seal::Private)
        .expect("a sealed folder in its place");
    let crashes = dir.join("crashes");
    assert!(crashes.is_dir() && !is_link(&crashes), "{held:?}");
    let moved = moved_in(&dir, "crashes.moved-");
    assert!(moved.len() == 1 && is_link(&moved[0]), "{moved:?}");
    assert!(names_in(&elsewhere).is_empty());
    assert!(!held.resealed());
}

/// T7b — a plain file where the leaf goes is moved aside whole, its content
/// kept, and a sealed folder made.
#[test]
fn a_file_where_the_leaf_goes_is_moved_aside() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("machine");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("crashes"), b"a file").unwrap();
    base_of(&dir)
        .sealed_dir("crashes", Seal::Private)
        .expect("a sealed folder in its place");
    assert!(dir.join("crashes").is_dir());
    let moved = moved_in(&dir, "crashes.moved-");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(std::fs::read(&moved[0]).unwrap(), b"a file");
}

/// T8 — an Administrators-owned folder with an inherited DACL and a file in
/// it: `sealed_dir` keeps the same folder and the file, and seals it.
#[test]
fn an_open_leaf_ours_is_sealed_in_place() {
    if !elevated_or_skip("T8: an open folder Administrators own") {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let crashes = root.path().join("crashes");
    std::fs::create_dir(&crashes).unwrap();
    std::fs::write(crashes.join("1-1.json"), b"{}").unwrap();
    assert!(icacls(&crashes, &["/setowner", "*S-1-5-32-544"]));
    assert!(!sealed(&sddl(&crashes), true), "inherited before");
    let held = base_of(root.path())
        .sealed_dir("crashes", Seal::Private)
        .expect("sealed in place");
    assert!(held.resealed());
    let shape = sddl(&crashes);
    assert!(sealed(&shape, true), "sealed: {shape}");
    assert_eq!(owner(&shape), "BA", "{shape}");
    assert_eq!(names_in(&crashes), ["1-1.json"], "the file is kept");
    assert!(moved_in(root.path(), "crashes.moved-").is_empty());

    // Sealed already: nothing more to do.
    let again = base_of(root.path())
        .sealed_dir("crashes", Seal::Private)
        .unwrap();
    assert!(!again.resealed());
    println!("{CHECKED}: T8");
}

/// T9 — `crashes` owned by another local account, with a file in it: moved
/// aside with its file and its owner; the new one sealed.
#[test]
fn a_leaf_another_account_owns_is_moved_aside() {
    if !elevated_or_skip("T9: a folder another account owns") {
        return;
    }
    let Some(guest) = local_guest() else {
        panic!(
            "T9 needs a local account to own the folder: this process runs as no account of this computer"
        );
    };
    let root = tempfile::tempdir().unwrap();
    let crashes = root.path().join("crashes");
    std::fs::create_dir(&crashes).unwrap();
    std::fs::write(crashes.join("keep.txt"), b"theirs").unwrap();
    assert!(icacls(&crashes, &["/setowner", &format!("*{guest}")]));
    base_of(root.path())
        .sealed_dir("crashes", Seal::Private)
        .expect("a sealed folder in its place");
    let moved = moved_in(root.path(), "crashes.moved-");
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(names_in(&moved[0]), ["keep.txt"]);
    let kept = owner(&sddl(&moved[0]));
    assert!(kept == guest || kept == "LG", "its owner is kept: {kept}");
    let shape = sddl(&crashes);
    assert!(sealed(&shape, true) && owner(&shape) == "BA", "{shape}");
    assert!(names_in(&crashes).is_empty());
    println!("{CHECKED}: T9");
}

/// T10 — a tree holding a junction: what it leads to is kept, `left` names
/// it, the root stays. Control: a tree with none is removed.
#[test]
fn a_tree_removal_follows_no_link_and_leaves_what_holds_one() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("dir");
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir_all(dir.join("t").join("sub")).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("keep.txt"), b"theirs").unwrap();
    std::fs::write(dir.join("t").join("sub").join("a.log"), b"x").unwrap();
    junction(&dir.join("t").join("sub").join("j"), &elsewhere);
    std::fs::create_dir_all(dir.join("u").join("sub")).unwrap();
    std::fs::write(dir.join("u").join("sub").join("a.log"), b"x").unwrap();
    std::fs::write(dir.join("u").join("b.log"), b"x").unwrap();
    let held = base_of(&dir);

    let dry = held.remove_tree("u", true).unwrap();
    assert!(dry.would_remove() && !dry.root_removed, "{dry:?}");
    assert!(
        dir.join("u").join("b.log").is_file(),
        "a dry run removes nothing"
    );

    let done = held.remove_tree("t", false).unwrap();
    assert!(elsewhere.join("keep.txt").is_file(), "{done:?}");
    assert!(!done.root_removed, "{done:?}");
    assert!(
        done.left.len() == 1 && named(&done.left[0].0, "j") && matches!(done.left[0].1, Why::Link),
        "{done:?}"
    );
    assert!(
        dir.join("t").join("sub").join("a.log").is_file(),
        "the whole tree is left"
    );

    let control = held.remove_tree("u", false).unwrap();
    assert!(
        control.root_removed && control.left.is_empty(),
        "{control:?}"
    );
    assert_eq!(control.removed_files, 2, "{control:?}");
    assert!(!dir.join("u").exists());

    let absent = held.remove_tree("v", false).unwrap();
    assert!(!absent.present && absent.left.is_empty(), "{absent:?}");
}

/// What this build does with `OBJ_DONT_REPARSE` (FR-145 records it): a name
/// with a junction in its middle, opened below a held folder with the flag.
/// Whatever the answer, the helper refuses the junction.
#[test]
fn obj_dont_reparse_is_measured_and_never_relied_on_alone() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("dir");
    let target = root.path().join("target");
    std::fs::create_dir(&dir).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("x"), b"x").unwrap();
    junction(&dir.join("j"), &target);
    let held = base_of(&dir);
    let answer = super::dont_reparse_answer(held.handle(), r"j\x");
    let build = std::process::Command::new(crate::appdirs::system32_tool("cmd.exe"))
        .args(["/c", "ver"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("OBJ_DONT_REPARSE honoured: {answer} ({build})");
    let refused = held.walk(&["j"]).expect_err("the junction is refused");
    assert!(matches!(refused.why, Why::Link), "{refused}");
}
