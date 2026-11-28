// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! #4500 — how this crate's tests bind: on loopback.
//!
//! On Windows the first UDP bind or TCP listen a program makes on an address
//! other than loopback asks the person at the machine whether to allow it.
//! Cargo names the lib's test binary `roomlerd-<hash>.exe`, so every new
//! target dir or rebuilt hash is a program Windows has never seen, and its
//! prompt reads as if the installed daemon were asking. So a test binds
//! through this module (docs/testing.md, "How tests bind"):
//!
//! * [`setting_engine`] and [`api`]: a webrtc-rs engine whose peers gather
//!   host candidates on loopback and open no mDNS socket
//!   (`tunnel_core::transport::webrtc_dc::confine_to_loopback`), and
//!   [`vnet_setting_engine`] for a peer on a vnet;
//! * the switches that production code passes its own values through where a
//!   test reaches it: [`rc_engine`], [`GATHER`] and [`any_v4`]. Outside this
//!   crate's own test build each is the identity, so a daemon on a real host
//!   binds exactly as before.
//!
//! A test that has to bind elsewhere (the socket webrtc-ice opens for a STUN
//! or TURN server, quinn-udp's GSO probe, mDNS) says why in a
//! `#[cfg_attr(windows, ignore = "...")]`: it runs on Linux CI and never in a
//! plain local run on Windows. CI holds the rest to it: the Windows lib suite
//! runs under scripts/test-binds-audit.ps1, which fails on any bind off
//! loopback.

use std::net::Ipv4Addr;

use tunnel_core::transport::webrtc_dc::Gather;
use webrtc::api::setting_engine::SettingEngine;

/// The remote-desktop session's engine, after production has set its own
/// filters on it: confined to loopback in this crate's tests, and returned
/// as it came anywhere else. The one vnet a session's engine can carry, the
/// L0 harness's hook (`peer::l0`), is installed by an integration test only,
/// where this is the identity: a loopback filter would leave a vnet no
/// address.
#[inline]
pub(crate) fn rc_engine(setting: SettingEngine) -> SettingEngine {
    #[cfg(test)]
    let setting = {
        let mut setting = setting;
        tunnel_core::transport::webrtc_dc::confine_to_loopback(&mut setting);
        setting
    };
    setting
}

/// Where a tunnel's peer gathers: every interface, and loopback in this
/// crate's tests.
pub(crate) const GATHER: Gather = if cfg!(test) {
    Gather::LoopbackOnly
} else {
    Gather::AllInterfaces
};

/// The address production binds an IPv4 socket on when any interface will
/// do, `0.0.0.0`: `127.0.0.1` in this crate's tests.
pub(crate) const fn any_v4() -> Ipv4Addr {
    if cfg!(test) {
        Ipv4Addr::LOCALHOST
    } else {
        Ipv4Addr::UNSPECIFIED
    }
}

/// A webrtc-rs engine for a test's own peer: host candidates on loopback,
/// no mDNS socket. On a vnet, take [`vnet_setting_engine`] instead: a
/// loopback filter leaves a vnet no address.
#[cfg(test)]
pub(crate) fn setting_engine() -> SettingEngine {
    let mut setting = SettingEngine::default();
    tunnel_core::transport::webrtc_dc::confine_to_loopback(&mut setting);
    setting
}

/// A webrtc-rs engine for a test's peer on a vnet, which the caller sets:
/// no mDNS socket, whose port is real on a vnet too, and no address filter,
/// since a vnet's addresses are virtual.
#[cfg(test)]
pub(crate) fn vnet_setting_engine() -> SettingEngine {
    let mut setting = SettingEngine::default();
    setting.set_ice_multicast_dns_mode(webrtc::ice::mdns::MulticastDnsMode::Disabled);
    setting
}

/// A webrtc-rs API over [`setting_engine`], for a test peer with no media
/// engine of its own.
#[cfg(test)]
pub(crate) fn api() -> webrtc::api::API {
    webrtc::api::APIBuilder::new()
        .with_setting_engine(setting_engine())
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outside a test the switches are production's own values; this build
    /// is a test, so they read loopback. The non-test half is the same
    /// expression with `cfg!(test)` false.
    #[test]
    fn in_a_test_every_switch_reads_loopback() {
        assert_eq!(any_v4(), Ipv4Addr::LOCALHOST);
        assert_eq!(GATHER, Gather::LoopbackOnly);
    }
}
