// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-3b-3 — the TCP ports this device listens on (decision 47), read
//! by the daemon from the system's own table when the owner turned on
//! `knowhow_scan_listeners`. No account's file is opened, so no account's
//! eyes are needed: the table is the machine's.
//!
//! | OS | Table | Its process |
//! |---|---|---|
//! | Linux | `/proc/net/tcp` and `tcp6`, state `0A` | the socket's inode under `/proc/<pid>/fd`, then `/proc/<pid>/comm` |
//! | macOS | `lsof -nP -iTCP -sTCP:LISTEN -F pctn` | its own lines |
//!
//! The daemon's own ports are left out: its loopback forwards are its
//! declared routes, which a scan reports as such. A daemon that is not root
//! sees only its own account's processes, so another account's listener
//! comes without its process, as `tunnel::port_holder` finds too.

use roomler_ai_remote_control::knowhow::{Listener, MAX_TEXT};

/// A listening socket as a table reads it: where it is bound, and the
/// process holding it when the table can say.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    address: String,
    port: u16,
    pid: Option<u32>,
    process: Option<String>,
}

/// The machine's TCP listeners, each bound address and port once, those the
/// daemon (`own`) holds left out.
pub(super) fn listeners(own: Option<u32>) -> Result<Vec<Listener>, String> {
    let mut out: Vec<Listener> = Vec::new();
    for row in table()? {
        if own.is_some() && row.pid == own {
            continue;
        }
        if out
            .iter()
            .any(|l| l.port == row.port && l.address == row.address)
        {
            continue;
        }
        out.push(Listener {
            port: row.port,
            protocol: "tcp".into(),
            address: row.address,
            process: row.process.filter(|p| !p.is_empty() && p.len() <= MAX_TEXT),
        });
    }
    out.sort_by(|a, b| (a.port, &a.address).cmp(&(b.port, &b.address)));
    Ok(out)
}

// ─── Linux: /proc/net/tcp names the socket inode, /proc/*/fd its owner ─────

#[cfg(target_os = "linux")]
fn table() -> Result<Vec<Row>, String> {
    let mut sockets = Vec::new();
    let mut read = false;
    for (path, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        // `tcp6` is absent where IPv6 is off.
        if let Ok(text) = std::fs::read_to_string(path) {
            read = true;
            sockets.extend(proc_net(&text, v6));
        }
    }
    if !read {
        return Err("the system's TCP table (/proc/net/tcp) cannot be read".into());
    }
    let owners = socket_owners(&sockets.iter().map(|s| s.2).collect());
    Ok(sockets
        .into_iter()
        .map(|(address, port, inode)| {
            let pid = owners.get(&inode).copied();
            let process = pid
                .and_then(|p| std::fs::read_to_string(format!("/proc/{p}/comm")).ok())
                .map(|c| c.trim().to_string());
            Row {
                address,
                port,
                pid,
                process,
            }
        })
        .collect())
}

/// The LISTENING sockets in a `/proc/net/tcp` or `tcp6` table: each one's
/// address, port and inode.
#[cfg(any(target_os = "linux", test))]
fn proc_net(table: &str, v6: bool) -> Vec<(String, u16, u64)> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.get(3) != Some(&"0A") {
                return None;
            }
            let (addr, port) = f.get(1)?.split_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            let address = if v6 { ipv6(addr)? } else { ipv4(addr)? };
            let inode = f.get(9)?.parse::<u64>().ok()?;
            Some((address, port, inode))
        })
        .collect()
}

/// The table spells an IPv4 address as one 32-bit word in host order: every
/// target this agent ships for is little-endian.
#[cfg(any(target_os = "linux", test))]
fn ipv4(hex: &str) -> Option<String> {
    let word = u32::from_str_radix(hex, 16).ok()?;
    Some(std::net::Ipv4Addr::from(word.to_le_bytes()).to_string())
}

/// ... and an IPv6 address as four such words.
#[cfg(any(target_os = "linux", test))]
fn ipv6(hex: &str) -> Option<String> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, chunk) in bytes.chunks_mut(4).enumerate() {
        let word = u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16).ok()?;
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    Some(std::net::Ipv6Addr::from(bytes).to_string())
}

/// Which process holds each socket inode, by a walk of `/proc/<pid>/fd`. A
/// process this daemon may not look into is passed by.
#[cfg(target_os = "linux")]
fn socket_owners(inodes: &std::collections::HashSet<u64>) -> std::collections::HashMap<u64, u32> {
    let mut owners = std::collections::HashMap::new();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return owners;
    };
    for entry in procs.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let inode = target
                .to_str()
                .and_then(|t| t.strip_prefix("socket:["))
                .and_then(|t| t.strip_suffix(']'))
                .and_then(|t| t.parse::<u64>().ok());
            if let Some(inode) = inode.filter(|i| inodes.contains(i)) {
                owners.entry(inode).or_insert(pid);
            }
        }
        if owners.len() == inodes.len() {
            break;
        }
    }
    owners
}

// ─── macOS: `lsof` names the listening processes ───────────────────────────

/// How long `lsof` may take: a scan never waits on it longer.
#[cfg(target_os = "macos")]
const LSOF_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(target_os = "macos")]
fn table() -> Result<Vec<Row>, String> {
    use std::process::{Command, Stdio};
    let child = Command::new("/usr/sbin/lsof")
        // -n -P: no DNS, no port names; -b: never block in the kernel;
        // -w: no warnings; -F: one field a line (p pid, c command, t type,
        // n name).
        .args(["-nP", "-b", "-w", "-iTCP", "-sTCP:LISTEN", "-F", "pctn"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("lsof: {e}"))?;
    let pid = child.id();
    // Its whole output read on a thread, so a long one never fills the pipe
    // while this waits on the exit.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(LSOF_LIMIT) {
        // `lsof` exits 1 when nothing matched: an empty table, not a failure.
        Ok(Ok(out)) => Ok(lsof_rows(&String::from_utf8_lossy(&out.stdout))),
        Ok(Err(e)) => Err(format!("lsof: {e}")),
        Err(_) => {
            // SAFETY: a signal to the child this function started.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            Err(format!(
                "lsof gave no answer within {}s",
                LSOF_LIMIT.as_secs()
            ))
        }
    }
}

/// `lsof -F pctn` output: a `p` line starts a process and `c` names it;
/// then, per socket, a `t` (`IPv4` or `IPv6`) and an `n` (`address:port`,
/// `*` for any address).
#[cfg(any(target_os = "macos", test))]
fn lsof_rows(text: &str) -> Vec<Row> {
    let (mut pid, mut process, mut v6) = (None, None, false);
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(tag) = line.chars().next() else {
            continue;
        };
        let value = &line[tag.len_utf8()..];
        match tag {
            'p' => {
                pid = value.parse().ok();
                process = None;
            }
            'c' => process = Some(value.to_string()),
            't' => v6 = value == "IPv6",
            'n' => {
                let Some((host, port)) = value.rsplit_once(':') else {
                    continue;
                };
                let Ok(port) = port.parse::<u16>() else {
                    continue;
                };
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let address = match host {
                    "*" if v6 => "::".to_string(),
                    "*" => "0.0.0.0".to_string(),
                    other => other.to_string(),
                };
                out.push(Row {
                    address,
                    port,
                    pid,
                    process: process.clone(),
                });
            }
            _ => {}
        }
    }
    out
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn table() -> Result<Vec<Row>, String> {
    Err("no reader of this system's TCP table".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    /// Real rows (`tunnel::port_holder`'s capture, WSL 2026-09-25), a
    /// listener on every address, and IPv6 ones on `::` and `::1`: only
    /// state `0A` is a listener, and a client's local port is not one.
    #[test]
    fn a_proc_net_table_names_its_listeners() {
        let v4 = format!(
            "{HEADER}\
             \x20  2: 0100007F:A40F 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 15473497 1 0000000000000000 100 0 0 10 0\n\
             \x20 11: 0100007F:A927 0100007F:A40F 01 00000000:00000000 00:00000000 00000000  1000        0 15473498 2 0000000000000000 20 0 0 10 -1\n\
             \x20  3: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 777 1 0000000000000000 100 0 0 10 0\n"
        );
        assert_eq!(
            proc_net(&v4, false),
            [
                ("127.0.0.1".to_string(), 41999, 15473497),
                ("0.0.0.0".to_string(), 8080, 777),
            ]
        );
        let v6 = format!(
            "{HEADER}\
             \x20  0: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0000000000000000 100 0 0 10 0\n\
             \x20  1: 00000000000000000000000001000000:0277 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4343 1 0000000000000000 100 0 0 10 0\n"
        );
        assert_eq!(
            proc_net(&v6, true),
            [("::".to_string(), 22, 4242), ("::1".to_string(), 631, 4343)]
        );
    }

    /// `lsof -F pctn`: each socket with its process, `*` spelled as the
    /// family's any-address.
    #[test]
    fn lsof_output_names_its_listeners() {
        let out = "p123\ncpostgres\ntIPv4\nn127.0.0.1:5432\ntIPv6\nn[::1]:5432\n\
                   p456\ncnginx\ntIPv4\nn*:80\ntIPv6\nn*:80\n";
        let rows: Vec<(String, u16, Option<u32>, Option<String>)> = lsof_rows(out)
            .into_iter()
            .map(|r| (r.address, r.port, r.pid, r.process))
            .collect();
        let pg = || Some("postgres".to_string());
        let nginx = || Some("nginx".to_string());
        assert_eq!(
            rows,
            [
                ("127.0.0.1".to_string(), 5432, Some(123), pg()),
                ("::1".to_string(), 5432, Some(123), pg()),
                ("0.0.0.0".to_string(), 80, Some(456), nginx()),
                ("::".to_string(), 80, Some(456), nginx()),
            ]
        );
    }

    /// A socket this test listens on is found, with its process, and left
    /// out when this process is the daemon whose own ports are not reported.
    #[test]
    fn a_live_listener_is_found_and_the_daemons_own_left_out() {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let found = listeners(None).unwrap();
        let mine = found
            .iter()
            .find(|l| l.port == port && l.address == "127.0.0.1")
            .expect("this test's own socket");
        assert_eq!(mine.protocol, "tcp");
        assert!(mine.process.is_some(), "{mine:?}");
        let without = listeners(Some(std::process::id())).unwrap();
        assert!(
            !without
                .iter()
                .any(|l| l.port == port && l.address == "127.0.0.1"),
            "the daemon's own port is left out"
        );
    }
}
