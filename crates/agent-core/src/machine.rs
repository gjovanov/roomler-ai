// SPDX-License-Identifier: MPL-2.0
// Copyright (C) 2026 G ROX EOOD
//! Stable machine fingerprint derivation.
//!
//! The server uses `(tenant_id, machine_id)` as a unique key, so `machine_id`
//! must be stable across restarts but unique per physical host. We derive a
//! SHA-256 of:
//!   - OS hostname (cheap, stable unless user renames the machine)
//!   - OS kind + arch (cheap, stable)
//!   - the config dir path (acts as a "user install" scope so one box can
//!     enroll multiple agent configs without colliding)
//!
//! For v1 this is good enough. A future refinement would read the DMI product
//! UUID on Linux / IOKit on macOS / WMI on Windows for a true hardware ID,
//! but those paths add platform-specific deps and root/admin requirements
//! we don't need yet.
//!
//! **FR-129 B2 — the macOS root half is that refinement, for one config.**
//! The setup wizard enrolls the root half as the person, who cannot read the
//! root config's own id, so it cannot reuse it; and on a Mac with no
//! `HostName` set the host name follows the network (a DHCP- or
//! Bonjour-derived name), so a reinstall on another network derived another
//! id and the server made another device row. [`root_half_machine_id`] takes
//! the root half's id from the hardware instead — the `IOPlatformUUID`,
//! System Information's "Hardware UUID", which any user can read with
//! `ioreg` — and, on a virtual Mac, the VM's own network address beside it,
//! because a VM cloned from another can carry the original's platform UUID
//! (see that function).

use sha2::{Digest, Sha256};
use std::path::Path;

pub fn derive_machine_id(config_path: &Path) -> String {
    let mut hasher = Sha256::new();

    if let Ok(host) = hostname() {
        hasher.update(host.as_bytes());
    }
    hasher.update(std::env::consts::OS.as_bytes());
    hasher.update(std::env::consts::ARCH.as_bytes());
    hasher.update(config_path.to_string_lossy().as_bytes());

    hex::encode(hasher.finalize())
}

/// OS hostname (POSIX `gethostname` on Unix, `COMPUTERNAME` env var
/// on Windows). Exposed for `logs_upload.rs` so the agent's uploader
/// can hash the hostname into `host_id_hash` per the rc.58 plan.
pub fn hostname() -> std::io::Result<String> {
    // `hostname` crate is another dep; keep things simple with `uname -n`
    // (POSIX) / Windows GetComputerName (via HOSTNAME env var).
    #[cfg(unix)]
    {
        // SAFETY: gethostname(3) takes a buffer and writes a NUL-terminated
        // string into it. 256 bytes is enough for POSIX-conformant hosts.
        let mut buf = [0u8; 256];
        let ret = unsafe { libc_gethostname(buf.as_mut_ptr() as *mut _, buf.len()) };
        if ret == 0 {
            let nul = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return Ok(String::from_utf8_lossy(&buf[..nul]).into_owned());
        }
        Err(std::io::Error::last_os_error())
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "hostname"))
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "gethostname"]
    fn libc_gethostname(name: *mut core::ffi::c_char, len: usize) -> i32;
}

// ─── FR-129 B2: the macOS root half's machine id, from the hardware ─────────

/// The domain the root half's id is hashed under: another product hashing the
/// same platform UUID gets another value, and a v2 rule can never collide
/// with a v1 id.
pub const ROOT_HALF_ID_TAG: &str = "roomler/macos-root-half/v1";

/// What this Mac reports about itself, for [`root_half_machine_id`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MacHardware {
    /// `IOPlatformUUID`, as `ioreg` prints it.
    pub platform_uuid: String,
    /// The Mac is a virtual machine: `kern.hv_vmm_present` is 1, or the model
    /// is a hypervisor's (`VirtualMac2,1` under Apple's Virtualization
    /// framework). Unknown counts as virtual — the stricter rule.
    pub virtual_machine: bool,
    /// The permanent link-layer address of `en0`. Read only for a virtual
    /// Mac: a physical Mac's id never depends on a network interface (a dock,
    /// an adapter, a private Wi-Fi address would move it).
    pub link_address: Option<String>,
}

/// The macOS root half's machine id: SHA-256 of [`ROOT_HALF_ID_TAG`] and the
/// `IOPlatformUUID`, and — on a virtual Mac only — the VM's permanent `en0`
/// address. 64 lowercase hex, the shape [`derive_machine_id`] makes.
///
/// * **Stable across reinstalls on the same Mac.** Neither the host name nor
///   the network enters it, and the platform UUID survives an OS reinstall.
/// * **Never the per-user half's.** That half's id hashes the host name, the
///   OS, the arch and the person's config path; this one hashes a tag no
///   other id carries. The two are two devices (FR-43: the hub keys sessions
///   on `agent_id`), and the server keeps them apart by these ids.
/// * **Never shared by two Macs.** A physical Mac's platform UUID is its
///   logic board's own.
/// * **A cloned VM does not inherit the original's id silently.** Apple's
///   Virtualization framework derives a guest's platform UUID from the VM's
///   machine identifier, and a clone that keeps the identifier — `tart clone`
///   copies it with the rest of the VM's configuration — reports the
///   original's UUID. What a clone does get is its own network address
///   (`tart clone` regenerates the MAC when the original is on the same
///   host, and every hypervisor does on a copy), so a virtual Mac's id hashes
///   that address too: the clone enrolls as a new device, and the fold
///   retires the key the original's row holds
///   (`enrollment::retire_key_of_previous_row`), with the previous row named
///   for the operator. A clone that keeps BOTH the identifier and the MAC is
///   byte-for-byte the same machine from inside, and nothing in the guest can
///   tell it apart; give it its own (`tart set --random-serial
///   --random-mac`).
///
/// An error when a fact it needs is missing; the caller then falls back to a
/// one-time id ([`resolve_root_half_machine_id`]) rather than to the host
/// name, which a clone shares too.
pub fn root_half_machine_id(hw: &MacHardware) -> Result<String, String> {
    let uuid = normalize_platform_uuid(&hw.platform_uuid).ok_or_else(|| {
        format!(
            "the platform UUID {:?} is not a hardware UUID",
            hw.platform_uuid
        )
    })?;
    let mut hasher = Sha256::new();
    hasher.update(ROOT_HALF_ID_TAG.as_bytes());
    hasher.update([0u8]);
    hasher.update(uuid.as_bytes());
    if hw.virtual_machine {
        let address = hw
            .link_address
            .as_deref()
            .and_then(normalize_link_address)
            .ok_or_else(|| {
                "this Mac is a virtual machine, and a VM's identity needs its own network \
                 address (en0's), which could not be read: a clone of it would otherwise \
                 enroll as the same device"
                    .to_string()
            })?;
        hasher.update([0u8]);
        hasher.update(address.as_bytes());
    }
    Ok(hex::encode(hasher.finalize()))
}

/// The root half's id, and how it was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootHalfId {
    pub machine_id: String,
    /// From the hardware ([`root_half_machine_id`]); `false` = a one-time id.
    pub from_hardware: bool,
    /// For the person, when the id is not the hardware's: why, and what a
    /// reinstall will then do.
    pub note: Option<String>,
}

/// The root half's id from what this Mac reported ([`read_mac_hardware`]).
/// When the hardware's id cannot be made, a ONE-TIME id from `random` — never
/// the host-name derivation, which a cloned VM shares and which moves with the
/// network: two machines on one device row is the failure that must not
/// happen, and a fresh row on a later reinstall (said here, and named by the
/// fold's previous-row warning) is the one that may.
pub fn resolve_root_half_machine_id(
    facts: Result<MacHardware, String>,
    random: impl FnOnce() -> [u8; 32],
) -> RootHalfId {
    match facts.and_then(|hw| root_half_machine_id(&hw)) {
        Ok(machine_id) => RootHalfId {
            machine_id,
            from_hardware: true,
            note: None,
        },
        Err(reason) => RootHalfId {
            machine_id: hex::encode(random()),
            from_hardware: false,
            note: Some(format!(
                "This Mac's hardware fingerprint could not be read ({reason}), so its \
                 private-network half enrolls under a one-time id: a later reinstall adds it \
                 as a new device, and the earlier one stays in the Devices list, offline, to \
                 remove."
            )),
        },
    }
}

/// `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`, uppercase — or `None` for anything
/// else, and for the all-zero UUID a platform reports when it has none.
pub fn normalize_platform_uuid(raw: &str) -> Option<String> {
    let s = raw.trim().to_ascii_uppercase();
    let groups: Vec<&str> = s.split('-').collect();
    let shape_ok = groups.len() == 5
        && groups
            .iter()
            .zip([8usize, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()));
    if !shape_ok || s.bytes().all(|b| b == b'0' || b == b'-') {
        return None;
    }
    Some(s)
}

/// `aa:bb:cc:dd:ee:ff`, lowercase, each byte two digits — or `None` for
/// anything else, and for the all-zero and broadcast addresses.
pub fn normalize_link_address(raw: &str) -> Option<String> {
    let parts: Vec<&str> = raw.trim().split(':').collect();
    if parts.len() != 6 {
        return None;
    }
    let mut bytes = [0u8; 6];
    for (slot, part) in bytes.iter_mut().zip(&parts) {
        if part.is_empty() || part.len() > 2 {
            return None;
        }
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    if bytes == [0u8; 6] || bytes == [0xffu8; 6] {
        return None;
    }
    Some(
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// The `IOPlatformUUID` from `ioreg -rd1 -c IOPlatformExpertDevice`, whose
/// line reads `"IOPlatformUUID" = "4B2E…"`.
pub fn parse_platform_uuid(ioreg: &str) -> Option<String> {
    ioreg.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim() != "\"IOPlatformUUID\"" {
            return None;
        }
        normalize_platform_uuid(value.trim().trim_matches('"'))
    })
}

/// The permanent address from `networksetup -getmacaddress en0`
/// (`Ethernet Address: aa:bb:… (Device: en0)`), or the current one from
/// `ifconfig en0` (`ether aa:bb:…`) — whichever the text is.
pub fn parse_link_address(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim();
        let rest = line
            .strip_prefix("Ethernet Address:")
            .or_else(|| line.strip_prefix("ether "))?;
        normalize_link_address(rest.split_whitespace().next()?)
    })
}

/// Is this a virtual Mac? From `sysctl -n kern.hv_vmm_present` (`1` / `0`)
/// and `sysctl -n hw.model`. `None` from both = unknown, which the caller
/// treats as virtual.
pub fn is_virtual_mac(hv_vmm_present: Option<&str>, model: Option<&str>) -> Option<bool> {
    let hv = hv_vmm_present.and_then(|v| match v.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    });
    let model = model.map(str::trim).filter(|m| !m.is_empty()).map(|m| {
        let lower = m.to_ascii_lowercase();
        lower.starts_with("virtualmac") || lower.contains("vmware") || lower.contains("parallels")
    });
    match (hv, model) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), _) | (None, Some(false)) => Some(false),
        (None, None) => None,
    }
}

/// Read this Mac's [`MacHardware`]: `ioreg`, `sysctl` and — on a virtual Mac
/// — `networksetup`, then `ifconfig`, by absolute path, as whoever runs this
/// (no privilege needed). Anywhere but macOS an error: there is no root half
/// to name there.
pub fn read_mac_hardware() -> Result<MacHardware, String> {
    #[cfg(target_os = "macos")]
    {
        fn run(program: &str, args: &[&str]) -> Option<String> {
            let out = std::process::Command::new(program)
                .args(args)
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        }
        let ioreg = run("/usr/sbin/ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])
            .ok_or_else(|| "ioreg did not answer".to_string())?;
        let platform_uuid = parse_platform_uuid(&ioreg)
            .ok_or_else(|| "ioreg named no IOPlatformUUID".to_string())?;
        let hv = run("/usr/sbin/sysctl", &["-n", "kern.hv_vmm_present"]);
        let model = run("/usr/sbin/sysctl", &["-n", "hw.model"]);
        let virtual_machine = is_virtual_mac(hv.as_deref(), model.as_deref()).unwrap_or(true);
        let link_address = if virtual_machine {
            run("/usr/sbin/networksetup", &["-getmacaddress", "en0"])
                .as_deref()
                .and_then(parse_link_address)
                .or_else(|| {
                    run("/sbin/ifconfig", &["en0"])
                        .as_deref()
                        .and_then(parse_link_address)
                })
        } else {
            None
        };
        Ok(MacHardware {
            platform_uuid,
            virtual_machine,
            link_address,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("not a Mac".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn is_deterministic_for_same_config_path() {
        let p = PathBuf::from("/tmp/test-roomler/config.toml");
        let a = derive_machine_id(&p);
        let b = derive_machine_id(&p);
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // sha256 hex
    }

    #[test]
    fn differs_for_different_config_paths() {
        let a = derive_machine_id(&PathBuf::from("/tmp/a.toml"));
        let b = derive_machine_id(&PathBuf::from("/tmp/b.toml"));
        assert_ne!(a, b);
    }

    // ── FR-129 B2: the root half's id ──

    const UUID_A: &str = "4B2E8D3C-1F6A-5E7B-9C0D-2A3B4C5D6E7F";
    const UUID_B: &str = "9F8E7D6C-5B4A-4321-8765-0FEDCBA98765";

    fn physical(uuid: &str) -> MacHardware {
        MacHardware {
            platform_uuid: uuid.into(),
            virtual_machine: false,
            link_address: None,
        }
    }

    fn vm(uuid: &str, mac: &str) -> MacHardware {
        MacHardware {
            platform_uuid: uuid.into(),
            virtual_machine: true,
            link_address: Some(mac.into()),
        }
    }

    /// Stable: the same Mac makes the same id, every time, in the shape every
    /// other machine id has — whatever the host name or the network is,
    /// since neither enters it.
    #[test]
    fn the_root_half_id_is_stable_and_hardware_only() {
        let a = root_half_machine_id(&physical(UUID_A)).unwrap();
        assert_eq!(a, root_half_machine_id(&physical(UUID_A)).unwrap());
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        // The UUID as ioreg prints it or in another case: one Mac, one id.
        assert_eq!(
            a,
            root_half_machine_id(&physical(&UUID_A.to_ascii_lowercase())).unwrap()
        );
        assert_eq!(
            a,
            root_half_machine_id(&physical(&format!(" {UUID_A}\n"))).unwrap()
        );
    }

    /// Never the per-user half's id, nor what `enroll` derives for the root
    /// config's path: the two halves are two devices.
    #[test]
    fn the_root_half_id_never_collides_with_a_derived_one() {
        let root = root_half_machine_id(&physical(UUID_A)).unwrap();
        for path in [
            "/Users/admin/Library/Application Support/live.roomler.roomler/config.toml",
            "/etc/roomler/config.toml",
        ] {
            assert_ne!(root, derive_machine_id(Path::new(path)), "{path}");
        }
        let vm_root = root_half_machine_id(&vm(UUID_A, "aa:bb:cc:dd:ee:01")).unwrap();
        assert_ne!(vm_root, root, "a VM's rule is not a physical Mac's");
    }

    /// Two Macs, two ids.
    #[test]
    fn two_macs_never_share_an_id() {
        assert_ne!(
            root_half_machine_id(&physical(UUID_A)).unwrap(),
            root_half_machine_id(&physical(UUID_B)).unwrap()
        );
    }

    /// A tart clone: the original's platform UUID, its own MAC. A different
    /// id — the clone is a new device, never the original's row. The same VM
    /// (same UUID, same MAC, the address in another spelling) keeps its id.
    #[test]
    fn a_cloned_vm_with_the_originals_uuid_gets_its_own_id() {
        let original = root_half_machine_id(&vm(UUID_A, "aa:bb:cc:dd:ee:01")).unwrap();
        let clone = root_half_machine_id(&vm(UUID_A, "aa:bb:cc:dd:ee:02")).unwrap();
        assert_ne!(original, clone);
        assert_eq!(
            original,
            root_half_machine_id(&vm(UUID_A, "AA:BB:CC:DD:EE:1")).unwrap(),
            "the same address however it is written"
        );
    }

    /// A physical Mac's id ignores the network: an adapter, a dock or a
    /// private Wi-Fi address must not move it.
    #[test]
    fn a_physical_macs_id_ignores_its_network_address() {
        let bare = root_half_machine_id(&physical(UUID_A)).unwrap();
        let with_mac = root_half_machine_id(&MacHardware {
            link_address: Some("aa:bb:cc:dd:ee:01".into()),
            ..physical(UUID_A)
        })
        .unwrap();
        assert_eq!(bare, with_mac);
    }

    /// A VM without its address, or a Mac without a hardware UUID, makes no
    /// hardware id — and the resolution then mints a one-time id and says
    /// so, never a shared or a host-name one.
    #[test]
    fn missing_facts_make_a_one_time_id_and_say_so() {
        for hw in [
            MacHardware {
                link_address: None,
                ..vm(UUID_A, "aa:bb:cc:dd:ee:01")
            },
            vm(UUID_A, "00:00:00:00:00:00"),
            physical("00000000-0000-0000-0000-000000000000"),
            physical("not-a-uuid"),
        ] {
            assert!(root_half_machine_id(&hw).is_err(), "{hw:?}");
        }

        let ok = resolve_root_half_machine_id(Ok(physical(UUID_A)), || unreachable!());
        assert!(ok.from_hardware && ok.note.is_none());
        assert_eq!(
            ok.machine_id,
            root_half_machine_id(&physical(UUID_A)).unwrap()
        );

        let one = resolve_root_half_machine_id(Err("ioreg did not answer".into()), || [7u8; 32]);
        assert!(!one.from_hardware);
        assert_eq!(one.machine_id, hex::encode([7u8; 32]));
        assert_eq!(one.machine_id.len(), 64);
        let note = one.note.expect("said");
        assert!(note.contains("ioreg did not answer"), "{note}");
        assert!(
            note.contains("one-time id") && note.contains("new device"),
            "{note}"
        );
        let two = resolve_root_half_machine_id(
            Ok(MacHardware {
                link_address: None,
                ..vm(UUID_A, "x")
            }),
            || [9u8; 32],
        );
        assert!(!two.from_hardware);
        assert_ne!(one.machine_id, two.machine_id);
        assert!(two.note.unwrap().contains("virtual machine"));
    }

    /// `ioreg -rd1 -c IOPlatformExpertDevice`, as a Mac prints it.
    #[test]
    fn the_platform_uuid_is_read_from_ioreg() {
        let ioreg = r##"+-o J316sAP  <class IOPlatformExpertDevice, id 0x100000201, registered, matched, active, busy 0 (2 ms), retain 39>
  {
    "IOPolledInterface" = "AppleARMWatchdogTimerHibernateHandler is not serializable"
    "#address-cells" = <02000000>
    "IOPlatformSerialNumber" = "C02XXXXXXXXX"
    "compatible" = <"J316sAP","MacBookPro18,1","AppleARM">
    "IOPlatformUUID" = "4b2e8d3c-1f6a-5e7b-9c0d-2a3b4c5d6e7f"
    "model" = <"MacBookPro18,1">
  }
"##;
        assert_eq!(parse_platform_uuid(ioreg).as_deref(), Some(UUID_A));
        assert_eq!(parse_platform_uuid("nothing here"), None);
        assert_eq!(
            parse_platform_uuid(r#""IOPlatformUUID" = "00000000-0000-0000-0000-000000000000""#),
            None
        );
        // A key that merely contains the name is not it.
        assert_eq!(
            parse_platform_uuid(&format!("\"IOPlatformUUIDx\" = \"{UUID_A}\"")),
            None
        );
    }

    /// `networksetup -getmacaddress en0` and `ifconfig en0`, as a Mac prints
    /// them.
    #[test]
    fn the_link_address_is_read_from_networksetup_or_ifconfig() {
        assert_eq!(
            parse_link_address("Ethernet Address: 3A:5C:1E:0B:9D:42 (Device: en0)\n").as_deref(),
            Some("3a:5c:1e:0b:9d:42")
        );
        let ifconfig = "en0: flags=8863<UP,BROADCAST,SMART,RUNNING,SIMPLEX,MULTICAST> mtu 1500\n\
                        \toptions=400<CHANNEL_IO>\n\
                        \tether 3a:5c:1e:b:9d:42\n\
                        \tinet 192.168.64.5 netmask 0xffffff00 broadcast 192.168.64.255\n";
        assert_eq!(
            parse_link_address(ifconfig).as_deref(),
            Some("3a:5c:1e:0b:9d:42")
        );
        assert_eq!(
            parse_link_address("Ethernet Address: N/A (Device: en0)"),
            None
        );
        assert_eq!(parse_link_address(""), None);
        assert_eq!(normalize_link_address("ff:ff:ff:ff:ff:ff"), None);
        assert_eq!(normalize_link_address("aa:bb:cc:dd:ee"), None);
        assert_eq!(normalize_link_address("aa:bb:cc:dd:ee:fff"), None);
    }

    /// The VM signal: either sysctl says so; unknown stays unknown (the
    /// caller reads that as virtual, the stricter rule).
    #[test]
    fn a_virtual_mac_is_told_from_a_physical_one() {
        assert_eq!(
            is_virtual_mac(Some("1\n"), Some("VirtualMac2,1")),
            Some(true)
        );
        assert_eq!(is_virtual_mac(Some("0"), Some("VirtualMac2,1")), Some(true));
        assert_eq!(is_virtual_mac(None, Some("VMware20,1")), Some(true));
        assert_eq!(
            is_virtual_mac(Some("1"), Some("MacBookPro18,1")),
            Some(true)
        );
        assert_eq!(is_virtual_mac(Some("0"), Some("Mac14,2")), Some(false));
        assert_eq!(is_virtual_mac(None, Some("Mac14,2")), Some(false));
        assert_eq!(is_virtual_mac(Some("0"), None), Some(false));
        assert_eq!(is_virtual_mac(None, None), None);
        assert_eq!(is_virtual_mac(Some("garbage"), Some("")), None);
    }

    /// On a real Mac (CI's macOS runner, itself a VM): the facts read, the
    /// UUID has its shape, and the id is the hardware's — or, on a VM whose
    /// en0 cannot be read, the reason says so.
    #[cfg(target_os = "macos")]
    #[test]
    fn this_mac_reports_its_hardware() {
        let hw = read_mac_hardware().expect("ioreg answers on a Mac");
        eprintln!(
            "this Mac: virtual={} link_address={}",
            hw.virtual_machine,
            hw.link_address.is_some()
        );
        assert!(
            normalize_platform_uuid(&hw.platform_uuid).is_some(),
            "{hw:?}"
        );
        match root_half_machine_id(&hw) {
            Ok(id) => assert_eq!(id.len(), 64),
            Err(e) => assert!(
                hw.virtual_machine && hw.link_address.is_none() && e.contains("virtual"),
                "{e}"
            ),
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn there_is_no_mac_hardware_off_a_mac() {
        assert!(read_mac_hardware().is_err());
    }
}
