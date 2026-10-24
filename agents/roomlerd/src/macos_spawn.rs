// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-97 P2f-2 — start a child that is its OWN responsible process.
//!
//! macOS asks a person about a privacy-guarded device (the microphone) on
//! behalf of the *responsible* process of whatever touches it. A supervised
//! Mac's GUI worker is started by the root daemon (`launchctl asuser` +
//! `sudo`, `macos_supervisor`), so the root daemon stays the responsible
//! process of everything the worker starts, and macOS never asks the person
//! anything for root: the recorder's microphone opened, no prompt appeared,
//! and three minutes of recording were silence (FR-97 §8, 2026-10-24).
//!
//! `posix_spawn` with `responsibility_spawnattrs_setdisclaim` makes the child
//! its own responsible process: the bundle's identity, running as the person,
//! in their session, so macOS asks them. The call is private libSystem API
//! (macOS 10.14+, what Chromium does for the same reason), so it is looked up
//! at run time and its absence is an answer ([`available`]), never a crash.
//!
//! The pipes are the ones `tokio::process` would make: stdin and stdout
//! piped, stderr inherited, so a caller reads to EOF and writes a stop exactly
//! as it does with any other recorder.

use std::ffi::{CString, OsString, c_char, c_int, c_void};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::OnceLock;

type SetDisclaim = unsafe extern "C" fn(*mut libc::posix_spawnattr_t, c_int) -> c_int;

/// The private call, looked up once.
fn set_disclaim() -> Option<SetDisclaim> {
    static FN: OnceLock<usize> = OnceLock::new();
    let addr = *FN.get_or_init(|| {
        // SAFETY: a lookup in the already-loaded images, by a NUL-terminated name.
        let sym = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                c"responsibility_spawnattrs_setdisclaim".as_ptr(),
            )
        };
        sym as usize
    });
    (addr != 0).then(|| {
        // SAFETY: the symbol is libSystem's, with this signature since 10.14.
        unsafe { std::mem::transmute::<*mut c_void, SetDisclaim>(addr as *mut c_void) }
    })
}

/// Whether this macOS can start a child that is its own responsible process.
pub fn available() -> bool {
    set_disclaim().is_some()
}

/// A started child: its pid, its stdin's write end, its stdout's read end.
pub struct Spawned {
    pub pid: libc::pid_t,
    pub stdin: OwnedFd,
    pub stdout: OwnedFd,
}

/// `exe args`, its own responsible process, the current environment with
/// `env` added on top; stdin and stdout piped, stderr inherited.
pub fn spawn_disclaimed(
    exe: &Path,
    args: &[OsString],
    env: &[(String, String)],
) -> std::io::Result<Spawned> {
    let disclaim = set_disclaim().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this macOS cannot start a child as its own responsible process",
        )
    })?;
    let nul = |b: &[u8]| {
        CString::new(b).map_err(|_| std::io::Error::other("an argument holds a NUL byte"))
    };
    let path = nul(exe.as_os_str().as_bytes())?;
    let mut argv_c = vec![path.clone()];
    for a in args {
        argv_c.push(nul(a.as_bytes())?);
    }
    let mut vars: std::collections::BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    for (k, v) in env {
        vars.insert(k.into(), v.into());
    }
    let mut envp_c = Vec::with_capacity(vars.len());
    for (k, v) in &vars {
        let mut b = k.as_bytes().to_vec();
        b.push(b'=');
        b.extend_from_slice(v.as_bytes());
        envp_c.push(nul(&b)?);
    }
    let mut argv: Vec<*mut c_char> = argv_c.iter().map(|c| c.as_ptr().cast_mut()).collect();
    argv.push(std::ptr::null_mut());
    let mut envp: Vec<*mut c_char> = envp_c.iter().map(|c| c.as_ptr().cast_mut()).collect();
    envp.push(std::ptr::null_mut());

    let (in_r, in_w) = pipe_cloexec()?;
    let (out_r, out_w) = pipe_cloexec()?;
    let mut pid: libc::pid_t = 0;
    // SAFETY: every pointer outlives the call; the attribute and file-action
    // objects are initialised before use and destroyed after; the child's ends
    // are dup'd onto 0 and 1, and every pipe end is close-on-exec, so the child
    // keeps only its stdio (stderr is ours, inherited).
    let rc = unsafe {
        let mut fa: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        libc::posix_spawn_file_actions_init(&mut fa);
        libc::posix_spawnattr_init(&mut attr);
        libc::posix_spawn_file_actions_adddup2(&mut fa, raw(&in_r), 0);
        libc::posix_spawn_file_actions_adddup2(&mut fa, raw(&out_w), 1);
        // What std's own spawn gives a child: no blocked signals, and SIGPIPE
        // back at its default, whatever the spawning thread had.
        let mut empty: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut empty);
        libc::posix_spawnattr_setsigmask(&mut attr, &empty);
        let mut defaults: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut defaults);
        libc::sigaddset(&mut defaults, libc::SIGPIPE);
        libc::posix_spawnattr_setsigdefault(&mut attr, &defaults);
        libc::posix_spawnattr_setflags(
            &mut attr,
            (libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF) as libc::c_short,
        );
        let d = disclaim(&mut attr, 1);
        let rc = if d != 0 {
            d
        } else {
            libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &fa,
                &attr,
                argv.as_ptr(),
                envp.as_ptr(),
            )
        };
        libc::posix_spawn_file_actions_destroy(&mut fa);
        libc::posix_spawnattr_destroy(&mut attr);
        rc
    };
    drop(in_r);
    drop(out_w);
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc));
    }
    Ok(Spawned {
        pid,
        stdin: in_w,
        stdout: out_r,
    })
}

fn raw(fd: &OwnedFd) -> RawFd {
    use std::os::fd::AsRawFd;
    fd.as_raw_fd()
}

/// A pipe whose two ends are close-on-exec (macOS has no `pipe2`).
fn pipe_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as c_int; 2];
    // SAFETY: `fds` is two ints, as `pipe` requires.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both were just returned by `pipe` and are ours alone.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    for fd in [raw(&r), raw(&w)] {
        // SAFETY: a valid descriptor we own.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((r, w))
}

/// Reap `pid` without blocking: `true` once it has exited (or is not ours).
pub fn try_reap(pid: libc::pid_t) -> bool {
    let mut status = 0;
    // SAFETY: a plain waitpid on a child we started.
    let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    r == pid || r == -1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn the_private_call_is_there_on_a_supported_mac() {
        // macOS 10.14+ has it; the agent's floor is far above that.
        assert!(
            available(),
            "responsibility_spawnattrs_setdisclaim is missing"
        );
    }

    #[test]
    fn a_disclaimed_child_pipes_both_ways_and_exits() {
        let s = spawn_disclaimed(Path::new("/bin/cat"), &[], &[]).expect("spawn");
        let mut stdin = std::fs::File::from(s.stdin);
        let mut stdout = std::fs::File::from(s.stdout);
        stdin
            .write_all(b"hello from the recorder's pipe\n")
            .unwrap();
        drop(stdin); // cat sees EOF and exits
        let mut out = String::new();
        stdout.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello from the recorder's pipe\n");
        for _ in 0..100 {
            if try_reap(s.pid) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("cat never exited");
    }

    #[test]
    fn the_extra_environment_reaches_the_child() {
        let s = spawn_disclaimed(
            Path::new("/usr/bin/printenv"),
            &["ROOMLER_P2F2_PROBE".into()],
            &[("ROOMLER_P2F2_PROBE".into(), "on-top".into())],
        )
        .expect("spawn");
        drop(std::fs::File::from(s.stdin));
        let mut out = String::new();
        std::fs::File::from(s.stdout)
            .read_to_string(&mut out)
            .unwrap();
        assert_eq!(out.trim(), "on-top");
        while !try_reap(s.pid) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
