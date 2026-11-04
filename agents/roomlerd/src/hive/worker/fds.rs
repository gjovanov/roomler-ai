// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P8-2a-2 — descriptors from the daemon to its worker, on the
//! worker's second channel (its fd 4): one byte each, with the descriptor
//! beside it (`SCM_RIGHTS`). A second socket, so that no frame of the first
//! ever has to be read together with what rides beside it.
//!
//! ⚠️ One way only: the daemon sends, the worker receives. A worker never
//! hands the daemon a descriptor, so a worker that went wrong cannot fill the
//! daemon's descriptor table.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use tokio::io::Interest;
use tokio::net::UnixStream;

/// The control buffer: room for one descriptor's message, `u64`-aligned as
/// `cmsghdr` needs, with room to spare for what a sender added wrongly.
type Control = [u64; 16];

/// Send `fd` across `ch`.
pub(crate) async fn send(ch: &UnixStream, fd: RawFd) -> std::io::Result<()> {
    loop {
        ch.writable().await?;
        match ch.try_io(Interest::WRITABLE, || send_now(ch.as_raw_fd(), fd)) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            r => return r,
        }
    }
}

/// The next descriptor across `ch`, close-on-exec. An error at the channel's
/// end, and for a byte that came without exactly one descriptor beside it.
pub(crate) async fn recv(ch: &UnixStream) -> std::io::Result<OwnedFd> {
    loop {
        ch.readable().await?;
        match ch.try_io(Interest::READABLE, || recv_now(ch.as_raw_fd())) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            r => return r,
        }
    }
}

fn send_now(sock: RawFd, fd: RawFd) -> std::io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control: Control = [0; 16];
    let size = std::mem::size_of::<RawFd>() as libc::c_uint;
    // SAFETY: a zeroed msghdr is valid; every pointer set below outlives the
    // sendmsg call, and the one control message fits the buffer.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(size) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        if c.is_null() {
            return Err(std::io::Error::other("no room for the descriptor"));
        }
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(size) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_NOSIGNAL;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        match libc::sendmsg(sock, &msg, flags) {
            n if n < 0 => Err(std::io::Error::last_os_error()),
            0 => Err(std::io::ErrorKind::WriteZero.into()),
            _ => Ok(()),
        }
    }
}

fn recv_now(sock: RawFd) -> std::io::Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut control: Control = [0; 16];
    // SAFETY: as in `send_now`; every descriptor the kernel installed is
    // taken into an `OwnedFd` below, so none is left open.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of::<Control>() as _;
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_CMSG_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let flags = 0;
        let n = libc::recvmsg(sock, &mut msg, flags);
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Every descriptor that came, owned, whatever else is wrong with them.
        let mut got: Vec<OwnedFd> = Vec::new();
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                let bytes = (*c).cmsg_len as usize - (data as usize - c as usize);
                for i in 0..bytes / std::mem::size_of::<RawFd>() {
                    got.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        if n == 0 && got.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the descriptor channel ended",
            ));
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(std::io::Error::other(
                "a descriptor's message was cut short",
            ));
        }
        if got.len() != 1 {
            return Err(std::io::Error::other(format!(
                "{} descriptors beside one byte, not one",
                got.len()
            )));
        }
        let fd = got.pop().expect("one");
        #[cfg(not(target_os = "linux"))]
        if libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;

    /// A listener sent across is the same listener on the other side: a
    /// connection to its address is accepted on the descriptor received,
    /// and the received copy is close-on-exec.
    #[tokio::test]
    async fn a_listener_sent_across_is_accepted_on_at_the_other_end() {
        let (a, b) = UnixStream::pair().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        send(&a, listener.as_raw_fd()).await.unwrap();
        drop(listener);
        let fd = recv(&b).await.unwrap();
        // SAFETY: fcntl on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0, "close-on-exec: {flags}");
        let got = std::net::TcpListener::from(fd);
        got.set_nonblocking(true).unwrap();
        let got = tokio::net::TcpListener::from_std(got).unwrap();
        let dial = tokio::spawn(tokio::net::TcpStream::connect(addr));
        let (_, peer) = tokio::time::timeout(std::time::Duration::from_secs(5), got.accept())
            .await
            .expect("accepted")
            .unwrap();
        assert!(peer.ip().is_loopback());
        dial.await.unwrap().unwrap();
    }

    /// Descriptors arrive in the order they were sent, a Unix listener as a
    /// TCP one; the channel's end is an error, not a descriptor.
    #[tokio::test]
    async fn descriptors_arrive_in_order_and_the_end_is_an_error() {
        let (a, b) = UnixStream::pair().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("e.sock");
        let unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_addr = tcp.local_addr().unwrap();
        send(&a, unix.as_raw_fd()).await.unwrap();
        send(&a, tcp.as_raw_fd()).await.unwrap();
        drop(a);
        let first = std::os::unix::net::UnixListener::from(recv(&b).await.unwrap());
        assert_eq!(
            first.local_addr().unwrap().as_pathname(),
            Some(path.as_path())
        );
        let second = std::net::TcpListener::from(recv(&b).await.unwrap());
        assert_eq!(second.local_addr().unwrap(), tcp_addr);
        let end = recv(&b).await;
        assert_eq!(
            end.map(|fd| fd.into_raw_fd()).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    /// A byte with no descriptor beside it is refused, not taken for one.
    #[tokio::test]
    async fn a_byte_with_no_descriptor_is_refused() {
        let (mut a, b) = UnixStream::pair().unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut a, &[0])
            .await
            .unwrap();
        let got = recv(&b).await;
        assert!(got.is_err(), "no descriptor came");
    }
}
