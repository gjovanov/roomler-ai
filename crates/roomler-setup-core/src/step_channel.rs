// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #3609 — the channel between a limited Roomler Setup and its administrator
//! step.
//!
//! A limited wizard crosses to the administrator once, for the step that runs
//! the installer (the app's `package_step`). On the background-service role
//! that step stays after the installer and writes the machine enrollment, which
//! the wizard enrolls in between. The two talk over a named pipe:
//!
//! - the wizard makes it before the prompt ([`Listener::create`]), named after
//!   the step's random stage ([`pipe_name`]): the first and only instance,
//!   local clients only, and SYSTEM and Administrators alone may open it, to
//!   read and write data (nobody may add an instance of their own);
//! - the wizard takes one connection only, the step's ([`Listener::accept`]):
//!   the client's process id must be the step's, from the handle the shell
//!   returned when it started the step, which the wizard holds;
//! - the step opens it ([`connect`]) to read and write data, with
//!   identification as its security quality of service: the wizard can tell
//!   who connected, and can never act as the step.
//!
//! One JSON object per line, at most [`MAX_LINE`] bytes. The messages are the
//! app's (`machine_write`); this module carries lines.

use std::io;

/// The longest line either side sends, its newline aside.
pub const MAX_LINE: usize = 64 * 1024;

/// The pipe of the step whose stage folder is named `stage`.
pub fn pipe_name(stage: &str) -> String {
    format!(r"\\.\pipe\roomler-setup-{stage}")
}

/// Read one line of at most [`MAX_LINE`] bytes, without its newline: the
/// step's side, which is synchronous. A peer that hung up before a whole line
/// is an `UnexpectedEof`; a longer line is `InvalidData`.
pub fn read_line<R: io::BufRead>(reader: &mut R) -> io::Result<String> {
    use std::io::{BufRead as _, Read as _};
    let mut line = String::new();
    let read = reader
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_line(&mut line)?;
    whole_line(read, line)
}

/// Send `line` and its newline, then flush. A line with a newline in it, or a
/// longer one than the other side takes, is refused unsent.
pub fn write_line<W: io::Write>(writer: &mut W, line: &str) -> io::Result<()> {
    fit_to_send(line)?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn fit_to_send(line: &str) -> io::Result<()> {
    if line.len() > MAX_LINE || line.contains('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not one line the channel takes",
        ));
    }
    Ok(())
}

/// What `read_line` read (`read` bytes into `line`), as one line or the
/// reason it is not one.
fn whole_line(read: usize, mut line: String) -> io::Result<String> {
    if read == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the other side hung up",
        ));
    }
    if !line.ends_with('\n') {
        return Err(if read > MAX_LINE {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "a line longer than the channel takes",
            )
        } else {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the other side hung up in the middle of a line",
            )
        });
    }
    line.pop();
    if line.ends_with('\r') {
        line.pop();
    }
    Ok(line)
}

#[cfg(target_os = "windows")]
pub use win::{Link, Listener, connect};

#[cfg(target_os = "windows")]
mod win {
    use std::io;
    use std::os::windows::io::AsRawHandle as _;
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::{ERROR_PIPE_BUSY, GENERIC_READ, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{FILE_WRITE_DATA, SECURITY_IDENTIFICATION};
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    use super::MAX_LINE;

    /// SYSTEM and Administrators may read it and write data to it
    /// (`0x12008b`), and nothing more: not `FILE_CREATE_PIPE_INSTANCE`, with
    /// which a process could add an instance of its own. Nobody else may open
    /// it at all.
    const PIPE_SDDL: &str = "D:P(A;;0x12008b;;;SY)(A;;0x12008b;;;BA)";

    /// How long the step keeps trying while the one instance is busy.
    const BUSY_WAIT: Duration = Duration::from_secs(10);

    /// A security descriptor made from SDDL, freed on drop.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Descriptor {
        fn from_sddl(sddl: &str) -> io::Result<Self> {
            let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: a NUL-terminated string; `sd` receives a LocalAlloc'd
            // descriptor, freed in Drop; the size out-param is optional.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut sd,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self(sd))
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: the one descriptor ConvertStringSecurityDescriptor… made.
            unsafe { LocalFree(self.0 as _) };
        }
    }

    /// The wizard's end, made before the prompt and not yet connected.
    pub struct Listener {
        server: NamedPipeServer,
    }

    impl Listener {
        /// Make the pipe `name` ([`super::pipe_name`]): the first and only
        /// instance, so a name something else holds is refused before the
        /// prompt, and local clients only. Needs a Tokio runtime.
        pub fn create(name: &str) -> io::Result<Self> {
            let sd = Descriptor::from_sddl(PIPE_SDDL)?;
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd.0,
                bInheritHandle: 0,
            };
            // SAFETY: the attributes and the descriptor they point to live
            // until the call returns; the system copies the descriptor into
            // the instance.
            let server = unsafe {
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .max_instances(1)
                    .reject_remote_clients(true)
                    .create_with_security_attributes_raw(name, (&raw mut attributes).cast())
            }?;
            Ok(Self { server })
        }

        /// Wait for process `pid`, the step, to connect. A connection from
        /// any other process is hung up on, and the wait goes on.
        pub async fn accept(self, pid: u32) -> io::Result<Link> {
            let server = self.server;
            loop {
                server.connect().await?;
                match client_pid(&server) {
                    Ok(client) if client == pid => {
                        return Ok(Link {
                            pipe: BufReader::new(server),
                        });
                    }
                    Ok(client) => tracing::warn!(
                        client,
                        step = pid,
                        "setup: a process other than the administrator step connected to its \
                         channel; it was hung up on"
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "setup: could not tell which process connected to the step's channel; \
                         it was hung up on"
                    ),
                }
                server.disconnect()?;
            }
        }
    }

    /// The process at the other end of a connected instance.
    fn client_pid(server: &NamedPipeServer) -> io::Result<u32> {
        let mut pid = 0u32;
        // SAFETY: a live pipe handle, and a u32 to write.
        let ok = unsafe { GetNamedPipeClientProcessId(server.as_raw_handle() as HANDLE, &mut pid) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(pid)
    }

    /// The wizard's end, connected to the step.
    pub struct Link {
        pipe: BufReader<NamedPipeServer>,
    }

    impl Link {
        /// Send one line ([`super::write_line`]'s rules).
        pub async fn send(&mut self, line: &str) -> io::Result<()> {
            super::fit_to_send(line)?;
            let pipe = self.pipe.get_mut();
            pipe.write_all(line.as_bytes()).await?;
            pipe.write_all(b"\n").await?;
            pipe.flush().await
        }

        /// Read one line ([`super::read_line`]'s rules).
        pub async fn recv(&mut self) -> io::Result<String> {
            let mut line = String::new();
            let read = (&mut self.pipe)
                .take(MAX_LINE as u64 + 1)
                .read_line(&mut line)
                .await?;
            super::whole_line(read, line)
        }
    }

    /// The step's end: open the wizard's pipe `name` to read and write data,
    /// identification only. A busy instance is retried for a few seconds;
    /// anything else, a pipe that is not there included, is the answer.
    pub fn connect(name: &str) -> io::Result<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt as _;
        let until = Instant::now() + BUSY_WAIT;
        loop {
            let opened = std::fs::OpenOptions::new()
                .access_mode(GENERIC_READ | FILE_WRITE_DATA)
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(name);
            match opened {
                Err(e)
                    if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                        && Instant::now() < until =>
                {
                    std::thread::sleep(Duration::from_millis(100));
                }
                other => return other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipe_is_named_after_its_stage() {
        assert_eq!(
            pipe_name("0123456789abcdef0123456789abcdef"),
            r"\\.\pipe\roomler-setup-0123456789abcdef0123456789abcdef"
        );
    }

    /// Lines in and out: the newline is the frame, a CR before it goes too,
    /// and nothing longer than the channel takes is read or sent.
    #[test]
    fn a_line_is_a_frame_and_a_long_one_is_refused() {
        let mut sent = Vec::new();
        write_line(&mut sent, r#"{"msi":{"code":0}}"#).unwrap();
        write_line(&mut sent, "second").unwrap();
        assert_eq!(sent, b"{\"msi\":{\"code\":0}}\nsecond\n");
        let mut reader = io::BufReader::new(&sent[..]);
        assert_eq!(read_line(&mut reader).unwrap(), r#"{"msi":{"code":0}}"#);
        assert_eq!(read_line(&mut reader).unwrap(), "second");
        let end = read_line(&mut reader).unwrap_err();
        assert_eq!(end.kind(), io::ErrorKind::UnexpectedEof);

        let mut reader = io::BufReader::new(&b"crlf\r\n"[..]);
        assert_eq!(read_line(&mut reader).unwrap(), "crlf");
        let mut reader = io::BufReader::new(&b"no newline"[..]);
        assert_eq!(
            read_line(&mut reader).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );

        let long = "x".repeat(MAX_LINE + 1);
        let bytes = format!("{long}\n").into_bytes();
        let mut reader = io::BufReader::new(bytes.as_slice());
        assert_eq!(
            read_line(&mut reader).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(write_line(&mut Vec::new(), &long).is_err());
        assert!(write_line(&mut Vec::new(), "two\nlines").is_err());
        let fits = "y".repeat(MAX_LINE);
        let mut sent = Vec::new();
        write_line(&mut sent, &fits).unwrap();
        assert_eq!(
            read_line(&mut io::BufReader::new(sent.as_slice())).unwrap(),
            fits
        );
    }

    /// The wizard takes the step's connection and no other, and the two
    /// exchange lines. Here the step is this process.
    #[cfg(target_os = "windows")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_wizard_takes_the_steps_connection_and_no_other() {
        use std::time::Duration;
        let name = pipe_name(&format!("test{:028x}", rand_part()));
        let listener = Listener::create(&name).expect("the pipe");
        assert!(
            Listener::create(&name).is_err(),
            "a name something holds is refused"
        );
        let me = std::process::id();

        // Another process id than the one connecting: hung up on, and the
        // wait goes on.
        let client = {
            let name = name.clone();
            std::thread::spawn(move || {
                let pipe = connect(&name).expect("connects");
                let mut reader = io::BufReader::new(pipe);
                read_line(&mut reader).map(|_| ())
            })
        };
        let waited =
            tokio::time::timeout(Duration::from_millis(500), listener.accept(me + 1)).await;
        assert!(waited.is_err(), "still waiting for the step");
        assert!(
            client.join().unwrap().is_err(),
            "the other process was hung up on"
        );

        // A name of its own: the first instance closes once the runtime has
        // seen its last wait cancelled.
        let name = pipe_name(&format!("test{:028x}", rand_part() ^ 1));
        let listener = Listener::create(&name).expect("another pipe");
        let step = {
            let name = name.clone();
            std::thread::spawn(move || {
                let pipe = connect(&name).expect("connects");
                let mut writer = pipe.try_clone().unwrap();
                let mut reader = io::BufReader::new(pipe);
                write_line(&mut writer, r#"{"msi":{"code":0}}"#).unwrap();
                let asked = read_line(&mut reader).unwrap();
                write_line(&mut writer, &format!("answer to {asked}")).unwrap();
            })
        };
        let mut link = listener.accept(me).await.expect("the step");
        assert_eq!(link.recv().await.unwrap(), r#"{"msi":{"code":0}}"#);
        link.send("\"prepare\"").await.unwrap();
        assert_eq!(link.recv().await.unwrap(), "answer to \"prepare\"");
        step.join().unwrap();
        assert_eq!(
            link.recv().await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof,
            "the step hung up"
        );
    }

    #[cfg(target_os = "windows")]
    fn rand_part() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            ^ u128::from(std::process::id())
    }
}
