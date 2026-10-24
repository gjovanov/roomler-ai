// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! FR-90 P4-5-1b — `knowhow_probe_reach`: where this device's knowhow probes
//! may go (decision 53). Its owner's list, empty by default, which means
//! nowhere: the sense SSH's `direct-tcpip` gives an empty `forward_acl`, since
//! a probe leaves from inside whatever network the device sits in.
//!
//! An entry is `HOST` or `HOST:PORTS`:
//!
//! | HOST | Matches |
//! |---|---|
//! | `10.0.0.0/8`, `100.64.0.7`, `[fd00::/8]`, `[::1]` | the addresses a probe connects to (an IPv6 one bracketed) |
//! | `db.example.com` | that name, asked of a probe |
//! | `*.example.com` | a name under it, never itself |
//!
//! `PORTS` is one port or a range (`8000-8999`), and every port when absent.
//! Each entry is a `forward_acl` rule (`DestinationRule`), TCP.
//!
//! ⚠️ An address is reached when it is **never probed by no rule**
//! ([`never_probed`]: link-local, the metadata addresses, …) **and** an
//! address rule holds it or a name rule matches the name the probe asked for.
//! A name rule trusts its name's own resolution: an owner who lists
//! `db.example.com` lets the probe reach wherever that name resolves.

use std::net::IpAddr;

use roomler_ai_remote_control::knowhow::{never_probed, probe_host};
use roomler_ai_remote_control::models::{
    DestinationRule, HostPattern, PortRange, ProtocolKind, dst_matches,
};

/// The most entries a device lists.
pub const MAX_ENTRIES: usize = 128;

/// One entry, read: its rule, or why not.
pub fn parse_entry(raw: &str) -> Result<DestinationRule, String> {
    let entry = raw.trim();
    if entry.is_empty() {
        return Err("an empty entry".into());
    }
    // `[v6 or v6 range]`, then an optional `:PORTS`; else the last `:`
    // splits the ports from a host that holds none.
    let (host, ports) = if let Some(rest) = entry.strip_prefix('[') {
        let (inside, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("{entry:?}: a `[` without its `]`"))?;
        let ports = match after {
            "" => None,
            p => Some(
                p.strip_prefix(':')
                    .ok_or_else(|| format!("{entry:?}: only `:PORTS` may follow the `]`"))?,
            ),
        };
        (inside, ports)
    } else {
        match entry.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p)),
            Some(_) => {
                return Err(format!(
                    "{entry:?}: an IPv6 address or range goes in brackets: [fd00::/8]:443"
                ));
            }
            None => (entry, None),
        }
    };
    let host_pattern = host_pattern(host).map_err(|why| format!("{entry:?}: {why}"))?;
    let port_range = match ports {
        None => PortRange {
            low: 1,
            high: 65535,
        },
        Some(p) => port_range(p).map_err(|why| format!("{entry:?}: {why}"))?,
    };
    Ok(DestinationRule {
        host_pattern,
        port_range,
        proto: ProtocolKind::Tcp,
    })
}

fn host_pattern(host: &str) -> Result<HostPattern, String> {
    if host.contains('/') {
        let net: ipnet::IpNet = host
            .parse()
            .map_err(|_| format!("{host:?} is not an address range"))?;
        return Ok(HostPattern::Cidr(net.trunc().to_string()));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        let net = ipnet::IpNet::from(ip);
        return Ok(HostPattern::Cidr(net.to_string()));
    }
    if let Some(suffix) = host.strip_prefix("*.") {
        let name = probe_host(suffix)
            .filter(|n| n.parse::<IpAddr>().is_err())
            .ok_or_else(|| format!("{host:?} is not `*.` and a DNS name"))?;
        return Ok(HostPattern::Wildcard(format!("*.{name}")));
    }
    let name = probe_host(host)
        .filter(|n| n.parse::<IpAddr>().is_err())
        .ok_or_else(|| format!("{host:?} is not an address, a range or a DNS name"))?;
    Ok(HostPattern::Exact(name))
}

fn port_range(raw: &str) -> Result<PortRange, String> {
    let port = |s: &str| {
        s.parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| format!("{s:?} is not a port from 1 to 65535"))
    };
    let (low, high) = match raw.split_once('-') {
        Some((a, b)) => (port(a)?, port(b)?),
        None => {
            let p = port(raw)?;
            (p, p)
        }
    };
    if low > high {
        return Err(format!("the range {low}-{high} runs backwards"));
    }
    Ok(PortRange { low, high })
}

/// An entry as the config keeps it: one spelling, so a list reads back as
/// what it allows.
pub fn fmt_entry(rule: &DestinationRule) -> String {
    let host = match &rule.host_pattern {
        HostPattern::Cidr(net) => match net.parse::<ipnet::IpNet>() {
            // A single address reads as itself.
            Ok(n) if n.prefix_len() == n.max_prefix_len() => match n.addr() {
                IpAddr::V4(a) => a.to_string(),
                IpAddr::V6(a) => format!("[{a}]"),
            },
            Ok(ipnet::IpNet::V6(n)) => format!("[{n}]"),
            _ => net.clone(),
        },
        HostPattern::Exact(name) | HostPattern::Wildcard(name) => name.clone(),
    };
    match (rule.port_range.low, rule.port_range.high) {
        (1, 65535) => host,
        (low, high) if low == high => format!("{host}:{low}"),
        (low, high) => format!("{host}:{low}-{high}"),
    }
}

/// A comma-separated list, as `roomler config set` gives it, read into the
/// config's spellings. Clearing it (`None` or empty) lists nothing, which
/// reaches nowhere: the fail-safe direction. One entry that does not read
/// refuses the whole list, so a typo never leaves it half applied.
pub fn parse_list(key: &str, value: Option<&str>) -> Result<Vec<String>, String> {
    let Some(raw) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = Vec::new();
    for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let rule = parse_entry(part).map_err(|why| format!("{key}: {why}"))?;
        let spelled = fmt_entry(&rule);
        if !out.contains(&spelled) {
            out.push(spelled);
        }
    }
    if out.len() > MAX_ENTRIES {
        return Err(format!("{key}: at most {MAX_ENTRIES} entries"));
    }
    Ok(out)
}

/// The list's rules, at use. An entry that no longer reads (a hand edit of
/// the file) is left out: it can only narrow what is reached, never widen it.
pub fn rules(entries: &[String]) -> Vec<DestinationRule> {
    entries
        .iter()
        .take(MAX_ENTRIES)
        .filter_map(|e| parse_entry(e).ok())
        .collect()
}

/// Whether a probe asked for `name` (none for an address) may connect to
/// `ip` on `port`: never an address [`never_probed`], and only one an
/// address rule holds, or reached by a name a name rule matches.
pub fn allows(rules: &[DestinationRule], name: Option<&str>, ip: IpAddr, port: u16) -> bool {
    if never_probed(ip) {
        return false;
    }
    let ip_text = ip.to_string();
    rules.iter().any(|rule| match &rule.host_pattern {
        HostPattern::Cidr(_) => dst_matches(rule, &ip_text, port),
        HostPattern::Exact(_) | HostPattern::Wildcard(_) => {
            name.is_some_and(|n| dst_matches(rule, n, port))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// Each form reads, and reads back in one spelling.
    #[test]
    fn an_entry_reads_and_reads_back_in_one_spelling() {
        for (raw, want) in [
            ("10.0.0.0/8", "10.0.0.0/8"),
            ("10.1.2.3/8", "10.0.0.0/8"),
            ("100.64.0.7", "100.64.0.7"),
            ("100.64.0.7:22", "100.64.0.7:22"),
            ("[fd00::/8]:443", "[fd00::/8]:443"),
            ("[::1]", "[::1]"),
            ("[::1]:8000-8999", "[::1]:8000-8999"),
            ("DB.Example.com:5432", "db.example.com:5432"),
            ("*.Example.com:443", "*.example.com:443"),
            (" roomler.ai:1-65535 ", "roomler.ai"),
        ] {
            let rule = parse_entry(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(fmt_entry(&rule), want, "{raw}");
            assert_eq!(rule.proto, ProtocolKind::Tcp);
            assert_eq!(
                parse_entry(want).unwrap(),
                rule,
                "{want} reads back as itself"
            );
        }
    }

    /// What is no entry is refused, with its reason.
    #[test]
    fn what_is_no_entry_is_refused() {
        for (raw, says) in [
            ("", "empty"),
            ("fd00::1:443", "brackets"),
            ("[fd00::1", "without its"),
            ("[fd00::1]443", "may follow"),
            ("10.0.0.0/33", "not an address range"),
            ("db.example.com:0", "1 to 65535"),
            ("db.example.com:70000", "1 to 65535"),
            ("db.example.com:9000-8000", "backwards"),
            ("https://db.example.com", "not a port"),
            ("*.10.0.0.1", "not `*.` and a DNS name"),
            ("a b:22", "not an address"),
        ] {
            let why = parse_entry(raw).unwrap_err();
            assert!(why.contains(says), "{raw:?}: {why}");
        }
    }

    /// A list is read whole or refused whole, duplicates once, and clearing
    /// it lists nothing.
    #[test]
    fn a_list_is_read_whole_or_refused_whole() {
        let list = parse_list(
            "knowhow_probe_reach",
            Some("100.64.0.0/10:22, roomler.ai:443,roomler.ai:443"),
        )
        .unwrap();
        assert_eq!(list, ["100.64.0.0/10:22", "roomler.ai:443"]);
        assert!(parse_list("knowhow_probe_reach", Some("roomler.ai:443,nope nope")).is_err());
        assert!(parse_list("knowhow_probe_reach", None).unwrap().is_empty());
        assert!(
            parse_list("knowhow_probe_reach", Some("  "))
                .unwrap()
                .is_empty()
        );
        let many: Vec<String> = (1..=MAX_ENTRIES + 1)
            .map(|p| format!("10.0.0.1:{p}"))
            .collect();
        assert!(parse_list("knowhow_probe_reach", Some(&many.join(","))).is_err());
        // At use, an entry a hand edit broke only narrows.
        let rules = rules(&["roomler.ai:443".into(), "broken entry".into()]);
        assert_eq!(rules.len(), 1);
    }

    /// Decision 53: an address rule holds what it holds, a name rule the
    /// name a probe asked for, each within its ports; never a never-probed
    /// address, whatever the list says; and an empty list nowhere.
    #[test]
    fn what_a_list_allows() {
        let list = rules(&[
            "100.64.0.0/10:22".into(),
            "db.example.com:5432".into(),
            "*.example.org:443".into(),
            "169.254.0.0/16".into(),
            "100.100.100.0/24".into(),
        ]);
        assert!(allows(&list, None, ip("100.64.0.7"), 22));
        assert!(
            !allows(&list, None, ip("100.64.0.7"), 23),
            "outside its ports"
        );
        assert!(allows(&list, Some("db.example.com"), ip("10.0.4.12"), 5432));
        assert!(
            !allows(&list, None, ip("10.0.4.12"), 5432),
            "the name's rule is not the address's"
        );
        assert!(!allows(
            &list,
            Some("cache.example.com"),
            ip("10.0.4.12"),
            5432
        ));
        assert!(allows(
            &list,
            Some("api.example.org"),
            ip("203.0.113.9"),
            443
        ));
        assert!(
            !allows(&list, Some("example.org"), ip("203.0.113.9"), 443),
            "a wildcard is never its own suffix"
        );
        assert!(
            !allows(&list, None, ip("169.254.169.254"), 80),
            "link-local, whatever the list says"
        );
        assert!(
            !allows(&list, None, ip("100.100.100.200"), 80),
            "the metadata address, whatever the list says"
        );
        assert!(allows(&list, None, ip("100.100.100.201"), 80));
        assert!(
            !allows(&[], None, ip("100.64.0.7"), 22),
            "an empty list reaches nowhere"
        );
    }
}
