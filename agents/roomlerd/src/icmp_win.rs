// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-134 — an ICMP echo with no `ping.exe`.
//!
//! The RTT prober asks every 15 s per peer, so shelling `ping` started about a
//! process a second on a host with 17 peers, each with a `conhost.exe`. These
//! are the calls `ping.exe` itself makes, so the path and the answer are the
//! same, and the round trip is the OS's own number.

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::ptr::{null, null_mut};
use std::time::Duration;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    ICMP_ECHO_REPLY, ICMPV6_ECHO_REPLY_LH, Icmp6CreateFile, Icmp6ParseReplies, Icmp6SendEcho2,
    IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET6, SOCKADDR_IN6};

/// `ping.exe`'s own default size.
const PAYLOAD: [u8; 32] = [b'a'; 32];
/// Room for the reply header, the echoed payload and the status block the v6
/// call needs, in `u64`s so the buffer is aligned for the reply structs.
const REPLY_WORDS: usize = 64;
const IP_SUCCESS: u32 = 0;

struct IcmpHandle(HANDLE);

impl IcmpHandle {
    fn new(h: HANDLE) -> Result<Self, String> {
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            // SAFETY: GetLastError is a thread-local read.
            return Err(format!("ICMP handle: win32 error {}", unsafe {
                GetLastError()
            }));
        }
        Ok(Self(h))
    }
}

impl Drop for IcmpHandle {
    fn drop(&mut self) {
        // SAFETY: we own the handle, and this closes it once.
        unsafe { IcmpCloseHandle(self.0) };
    }
}

fn echo_v4(dst: Ipv4Addr, timeout_ms: u32) -> Result<u32, String> {
    // SAFETY: takes no arguments; the result is checked.
    let h = IcmpHandle::new(unsafe { IcmpCreateFile() })?;
    let mut reply = [0u64; REPLY_WORDS];
    // SAFETY: the payload and the reply buffer outlive the call, and the sizes
    // passed are theirs. The address is in network byte order, as the call takes.
    let answers = unsafe {
        IcmpSendEcho(
            h.0,
            u32::from_ne_bytes(dst.octets()),
            PAYLOAD.as_ptr().cast::<c_void>(),
            PAYLOAD.len() as u16,
            null(),
            reply.as_mut_ptr().cast::<c_void>(),
            size_of::<[u64; REPLY_WORDS]>() as u32,
            timeout_ms,
        )
    };
    if answers == 0 {
        return Err("no reply".into());
    }
    // SAFETY: a non-zero return means the buffer starts with a reply, and the
    // buffer is aligned and larger than the struct.
    let answer = unsafe { &*reply.as_ptr().cast::<ICMP_ECHO_REPLY>() };
    if answer.Status != IP_SUCCESS {
        return Err("no reply".into());
    }
    Ok(answer.RoundTripTime)
}

fn sockaddr_v6(ip: Ipv6Addr) -> SOCKADDR_IN6 {
    // SAFETY: SOCKADDR_IN6 is plain data, and all zero is a valid value.
    let mut a: SOCKADDR_IN6 = unsafe { zeroed() };
    a.sin6_family = AF_INET6;
    a.sin6_addr.u.Byte = ip.octets();
    a
}

fn echo_v6(dst: Ipv6Addr, timeout_ms: u32) -> Result<u32, String> {
    // SAFETY: takes no arguments; the result is checked.
    let h = IcmpHandle::new(unsafe { Icmp6CreateFile() })?;
    let source = sockaddr_v6(Ipv6Addr::UNSPECIFIED);
    let dest = sockaddr_v6(dst);
    let mut reply = [0u64; REPLY_WORDS];
    // SAFETY: both addresses, the payload and the reply buffer outlive the call.
    // No event and no APC makes it synchronous.
    let answers = unsafe {
        Icmp6SendEcho2(
            h.0,
            null_mut(),
            None,
            null(),
            &source,
            &dest,
            PAYLOAD.as_ptr().cast::<c_void>(),
            PAYLOAD.len() as u16,
            null(),
            reply.as_mut_ptr().cast::<c_void>(),
            size_of::<[u64; REPLY_WORDS]>() as u32,
            timeout_ms,
        )
    };
    if answers == 0 {
        return Err("no reply".into());
    }
    // SAFETY: the buffer is the one the call filled, at the size it was given.
    let parsed = unsafe {
        Icmp6ParseReplies(
            reply.as_mut_ptr().cast::<c_void>(),
            size_of::<[u64; REPLY_WORDS]>() as u32,
        )
    };
    if parsed == 0 {
        return Err("no reply".into());
    }
    // SAFETY: as in `echo_v4`.
    let answer = unsafe { &*reply.as_ptr().cast::<ICMPV6_ECHO_REPLY_LH>() };
    if answer.Status != IP_SUCCESS {
        return Err("no reply".into());
    }
    Ok(answer.RoundTripTime)
}

/// One echo to `dst`. Blocks for up to `timeout`, so call it off the runtime.
/// Under 1 ms reads as 1 ms, which is what parsing `ping`'s `time<1ms` gave.
pub fn echo(dst: IpAddr, timeout: Duration) -> Result<Duration, String> {
    let ms = timeout.as_millis().clamp(100, u128::from(u32::MAX)) as u32;
    let rtt = match dst {
        IpAddr::V4(v4) => echo_v4(v4, ms)?,
        IpAddr::V6(v6) => echo_v6(v6, ms)?,
    };
    Ok(Duration::from_millis(u64::from(rtt.max(1))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_answers_on_both_families() {
        let t = Duration::from_secs(2);
        assert!(echo(IpAddr::V4(Ipv4Addr::LOCALHOST), t).is_ok());
        assert!(echo(IpAddr::V6(Ipv6Addr::LOCALHOST), t).is_ok());
    }

    #[test]
    fn an_address_nobody_holds_is_no_reply_not_a_panic() {
        // TEST-NET-1 is reserved and never routed.
        let quiet = "192.0.2.1".parse().unwrap();
        assert!(echo(quiet, Duration::from_millis(200)).is_err());
    }
}
