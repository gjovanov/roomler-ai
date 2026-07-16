//! Windows Filtering Platform (WFP) direct programming for the L3 overlay.
//!
//! On a corporate host whose Windows Defender Firewall is locked down by
//! Group Policy (`AllowLocalFirewallRules=False`), an unsolicited inbound
//! packet to the overlay's `roomler` adapter is dropped — and a local
//! `New-NetFirewallRule` is ignored. The overlay's relay, WireGuard, and
//! routing all work; the host just won't *answer* inbound.
//!
//! Tailscale solves this by programming WFP directly from its LocalSystem
//! service rather than adding Defender rules. We do the same: install a
//! **high-weight, hard-permit sublayer** scoped to the `roomler` adapter's
//! interface LUID across the four ALE V4/V6 layers. A *hard* permit
//! (`FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT`) in a sublayer at weight `0xFFFE`
//! (above the MPSSVC firewall sublayer's ~weight 2) overrides a GPO
//! *filter* block (empirically: simplewall #689).
//!
//! **Limits (honest):** this beats a filter-based GPO block but **cannot**
//! beat a callout-driver veto or an IPsec connection-security rule (some
//! EDR/ZTNA agents). For those the only recourse is an IT-managed firewall
//! exception. The install is **best-effort**: a failure logs a WARN and
//! the overlay still comes up (it only matters on hosts where the firewall
//! is the problem).
//!
//! Lifetime: the session is opened `FWPM_SESSION_FLAG_DYNAMIC`, so every
//! object we add is auto-removed by BFE when the engine handle closes or
//! the process exits — robust to a crash, no stale rules. [`WfpGuard`]
//! owns the handle and rides [`super::tun::SystemTun`]'s lifetime.
//!
//! See `docs/overlay-wfp.md`. This pattern reads as "WFP tampering" to
//! some EDRs — it is **additive permit only**, LUID-scoped to `roomler`,
//! never shields-up, and break-glass-disable-able via
//! `ROOMLER_AGENT_WFP_PERMIT=0`.
#![cfg(all(feature = "overlay-l3", windows))]

use windows_sys::Win32::Foundation::{FWP_E_ALREADY_EXISTS, HANDLE};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::*;
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_WINNT;
use windows_sys::core::GUID;

/// Stable identity for our WFP objects so they're greppable in
/// `netsh wfp show filters` and recognizable if a dynamic session ever
/// leaks. ASCII "ROOMLER\0PROVIDER" / "ROOMLER\0SUBLAYER" as u128 — no
/// significance beyond being two fixed, distinct GUIDs.
const PROVIDER_GUID: GUID = GUID::from_u128(0x524f4f4d_4c45_5200_5052_4f5649444552);
const SUBLAYER_GUID: GUID = GUID::from_u128(0x524f4f4d_4c45_5200_5355_424c41594552);

const PROVIDER_NAME: &str = "Roomler Overlay";
const SUBLAYER_NAME: &str = "Roomler Overlay Permit (LUID-scoped)";
const FILTER_NAME: &str = "Roomler Overlay Inbound/Outbound Permit";

/// Sublayer weight. Above the MPSSVC firewall sublayer (~weight 2) so our
/// sublayer is arbitrated first; just below `0xFFFF` to leave headroom.
const SUBLAYER_WEIGHT: u16 = 0xFFFE;

/// The four ALE layers we bind hard-permit filters to. RECV_ACCEPT is the
/// one that beats an *inbound* GPO drop; CONNECT is cheap outbound insurance.
const LAYERS: [GUID; 4] = [
    FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4,
    FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6,
    FWPM_LAYER_ALE_AUTH_CONNECT_V4,
    FWPM_LAYER_ALE_AUTH_CONNECT_V6,
];

/// `ROOMLER_AGENT_WFP_PERMIT` — default **ON** for `overlay-l3`. Set to
/// `0`/`false`/`no`/`off` (case-insensitive) to skip WFP programming, e.g.
/// on a host where IT installed a managed exception, or to silence an
/// AV "firewall tampering" alert. Matches the agent's truthy convention.
pub fn wfp_enabled() -> bool {
    match crate::env::node_env("WFP_PERMIT") {
        Some(v) => {
            let t = v.trim();
            !(t.eq_ignore_ascii_case("0")
                || t.eq_ignore_ascii_case("false")
                || t.eq_ignore_ascii_case("no")
                || t.eq_ignore_ascii_case("off"))
        }
        None => true,
    }
}

/// What went wrong installing the permit. The messages are operator-facing
/// (logged at WARN) and point at the IT-exception fallback.
#[derive(Debug, thiserror::Error)]
pub enum WfpError {
    #[error(
        "WFP engine open failed (err={0:#x}); is the Base Filtering Engine (BFE) service running and are we LocalSystem?"
    )]
    EngineOpen(u32),
    #[error(
        "WFP filter add/commit failed (err={0:#x}); LocalSystem rights, a callout-driver veto, or an IPsec connection-security GPO may be blocking — request an IT-managed firewall exception for the roomler adapter"
    )]
    Commit(u32),
}

/// Owns a dynamic WFP session. Dropping it closes the engine handle, which
/// makes BFE reap the provider + sublayer + all filters we added.
pub struct WfpGuard {
    engine: HANDLE,
}

// The engine handle is a kernel HANDLE; we never share a `&mut` to it and
// `FwpmEngineClose0` is safe to call from any thread. The guard is moved
// into `SystemTun` and dropped on the runtime task.
unsafe impl Send for WfpGuard {}
unsafe impl Sync for WfpGuard {}

/// UTF-16, null-terminated, for a WFP `displayData.name` (PWSTR). The
/// returned `Vec` must outlive the FFI call that borrows its pointer.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Build one hard-permit filter for `layer`, scoped to the interface in
/// `condition` (a single `IP_LOCAL_INTERFACE == luid` condition). Pure —
/// no FFI — so the hard-permit semantics are unit-testable without BFE.
/// `provider_key`, `condition`, and `name` must outlive the `FwpmFilterAdd0`
/// call that copies this struct.
fn build_filter(
    layer: GUID,
    provider_key: *mut GUID,
    condition: *mut FWPM_FILTER_CONDITION0,
    name: *mut u16,
) -> FWPM_FILTER0 {
    // SAFETY: zeroed is a valid all-null/empty WFP filter; we set the
    // fields that matter below. The unions default to a zero variant.
    let mut f: FWPM_FILTER0 = unsafe { std::mem::zeroed() };
    f.displayData.name = name;
    f.providerKey = provider_key;
    f.layerKey = layer;
    f.subLayerKey = SUBLAYER_GUID;
    // Auto weight within our sublayer (FWP_EMPTY) — our sublayer's high
    // weight is what wins arbitration, not the per-filter weight.
    f.weight.r#type = FWP_EMPTY;
    f.numFilterConditions = 1;
    f.filterCondition = condition;
    f.action.r#type = FWP_ACTION_PERMIT;
    // HARD permit: clears the action-write right so a lower-weight GPO
    // filter block can't overwrite our decision. This is the bit that
    // actually beats the firewall.
    f.flags = FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT;
    f
}

impl WfpGuard {
    /// Install the LUID-scoped hard-permit filters for the `roomler`
    /// adapter. `luid` comes from `tun::AbstractDeviceExt::tun_luid()`.
    pub fn install(luid: u64) -> Result<Self, WfpError> {
        // Backing storage that must outlive the FFI calls below.
        let provider_name = wide(PROVIDER_NAME);
        let sublayer_name = wide(SUBLAYER_NAME);
        let filter_name = wide(FILTER_NAME);
        let mut provider_key = PROVIDER_GUID;
        let mut luid_val: u64 = luid;

        // SAFETY: every pointer handed to the WFP API below points at a
        // local that outlives the call; the API copies the data. Structs
        // are zero-initialized then have only valid fields set.
        unsafe {
            // --- Open a DYNAMIC engine session (auto-cleanup on close). ---
            let mut session: FWPM_SESSION0 = std::mem::zeroed();
            session.displayData.name = provider_name.as_ptr() as *mut u16;
            session.flags = FWPM_SESSION_FLAG_DYNAMIC;
            let mut engine: HANDLE = std::ptr::null_mut();
            let rc = FwpmEngineOpen0(
                std::ptr::null(),
                RPC_C_AUTHN_WINNT,
                std::ptr::null(),
                &session,
                &mut engine,
            );
            if rc != 0 {
                return Err(WfpError::EngineOpen(rc));
            }

            // From here on, any failure aborts the txn + closes the engine.
            let fail = |engine: HANDLE, rc: u32| -> WfpError {
                FwpmTransactionAbort0(engine);
                FwpmEngineClose0(engine);
                WfpError::Commit(rc)
            };

            let rc = FwpmTransactionBegin0(engine, 0);
            if rc != 0 {
                return Err(fail(engine, rc));
            }

            // --- Provider (bookkeeping/owner tag). ---
            let mut provider: FWPM_PROVIDER0 = std::mem::zeroed();
            provider.providerKey = PROVIDER_GUID;
            provider.displayData.name = provider_name.as_ptr() as *mut u16;
            let rc = FwpmProviderAdd0(engine, &provider, std::ptr::null_mut());
            if rc != 0 && rc != FWP_E_ALREADY_EXISTS as u32 {
                return Err(fail(engine, rc));
            }

            // --- Sublayer at weight 0xFFFE (arbitrated before MPSSVC). ---
            let mut sublayer: FWPM_SUBLAYER0 = std::mem::zeroed();
            sublayer.subLayerKey = SUBLAYER_GUID;
            sublayer.displayData.name = sublayer_name.as_ptr() as *mut u16;
            sublayer.providerKey = &mut provider_key;
            sublayer.weight = SUBLAYER_WEIGHT;
            let rc = FwpmSubLayerAdd0(engine, &sublayer, std::ptr::null_mut());
            if rc != 0 && rc != FWP_E_ALREADY_EXISTS as u32 {
                return Err(fail(engine, rc));
            }

            // --- One hard-permit filter per ALE layer, LUID-scoped. ---
            let mut condition: FWPM_FILTER_CONDITION0 = std::mem::zeroed();
            condition.fieldKey = FWPM_CONDITION_IP_LOCAL_INTERFACE;
            condition.matchType = FWP_MATCH_EQUAL;
            condition.conditionValue.r#type = FWP_UINT64;
            condition.conditionValue.Anonymous.uint64 = &mut luid_val;

            for layer in LAYERS {
                let filter = build_filter(
                    layer,
                    &mut provider_key,
                    &mut condition,
                    filter_name.as_ptr() as *mut u16,
                );
                let mut id: u64 = 0;
                let rc = FwpmFilterAdd0(engine, &filter, std::ptr::null_mut(), &mut id);
                if rc != 0 {
                    return Err(fail(engine, rc));
                }
            }

            let rc = FwpmTransactionCommit0(engine);
            if rc != 0 {
                return Err(fail(engine, rc));
            }

            Ok(Self { engine })
        }
    }
}

impl Drop for WfpGuard {
    fn drop(&mut self) {
        if !self.engine.is_null() {
            // SAFETY: `engine` is a live handle from FwpmEngineOpen0; closing
            // a dynamic session reaps every object we added.
            unsafe {
                FwpmEngineClose0(self.engine);
            }
        }
    }
}

/// Fallback: resolve an adapter alias (e.g. `"roomler"`) to its `NET_LUID`
/// value. Only needed if a future device type doesn't expose the LUID
/// directly — `SystemTun` uses `tun_luid()`. Returns the raw `u64`.
#[allow(dead_code)]
pub fn luid_from_alias(alias: &str) -> Result<u64, WfpError> {
    use windows_sys::Win32::NetworkManagement::IpHelper::ConvertInterfaceAliasToLuid;
    use windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH;

    let walias = wide(alias);
    let mut luid: NET_LUID_LH = unsafe { std::mem::zeroed() };
    // SAFETY: `walias` is a valid null-terminated PCWSTR; `luid` is a valid
    // out-param. Non-zero return is a WIN32_ERROR.
    let rc = unsafe { ConvertInterfaceAliasToLuid(walias.as_ptr(), &mut luid) };
    if rc != 0 {
        return Err(WfpError::EngineOpen(rc));
    }
    Ok(unsafe { luid.Value })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reconstruct a `u128` from a `windows-sys` `GUID` (which exposes
    /// neither `to_u128` nor `PartialEq`) — inverse of `GUID::from_u128`.
    fn guid_u128(g: GUID) -> u128 {
        ((g.data1 as u128) << 96)
            | ((g.data2 as u128) << 80)
            | ((g.data3 as u128) << 64)
            | (u64::from_be_bytes(g.data4) as u128)
    }

    #[test]
    fn provider_and_sublayer_guids_are_distinct() {
        assert_ne!(
            guid_u128(PROVIDER_GUID),
            guid_u128(SUBLAYER_GUID),
            "provider and sublayer GUIDs must differ"
        );
    }

    #[test]
    fn guids_are_stable_golden_values() {
        // Lock the identity so a refactor can't silently rotate it (the
        // values are what operators grep for in `netsh wfp show filters`).
        assert_eq!(
            guid_u128(PROVIDER_GUID),
            0x524f4f4d_4c45_5200_5052_4f5649444552
        );
        assert_eq!(
            guid_u128(SUBLAYER_GUID),
            0x524f4f4d_4c45_5200_5355_424c41594552
        );
    }

    #[test]
    fn sublayer_weight_is_above_mpssvc() {
        // MPSSVC firewall sublayer is ~weight 2; ours must dominate so it's
        // arbitrated first. 0xFFFE leaves 0xFFFF as headroom, and is far
        // above the MPSSVC band.
        assert_eq!(SUBLAYER_WEIGHT, 0xFFFE);
    }

    #[test]
    fn wfp_enabled_parses_truthy_table() {
        // Serialize env mutation across the table.
        let key = "ROOMLER_AGENT_WFP_PERMIT";
        let restore = std::env::var(key).ok();

        unsafe { std::env::remove_var(key) };
        assert!(wfp_enabled(), "unset → default ON");

        for v in ["0", "false", "FALSE", "no", "Off", " off "] {
            unsafe { std::env::set_var(key, v) };
            assert!(!wfp_enabled(), "{v:?} → disabled");
        }
        for v in ["1", "true", "yes", "on", "anything-else"] {
            unsafe { std::env::set_var(key, v) };
            assert!(wfp_enabled(), "{v:?} → enabled");
        }

        match restore {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    #[test]
    fn build_filter_locks_hard_permit_semantics() {
        let mut provider_key = PROVIDER_GUID;
        let mut luid_val: u64 = 0x0123_4567_89ab_cdef;
        let mut condition: FWPM_FILTER_CONDITION0 = unsafe { std::mem::zeroed() };
        condition.fieldKey = FWPM_CONDITION_IP_LOCAL_INTERFACE;
        condition.matchType = FWP_MATCH_EQUAL;
        condition.conditionValue.r#type = FWP_UINT64;
        condition.conditionValue.Anonymous.uint64 = &mut luid_val;
        let mut name = wide(FILTER_NAME);

        let f = build_filter(
            FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4,
            &mut provider_key,
            &mut condition,
            name.as_mut_ptr(),
        );

        assert_eq!(f.action.r#type, FWP_ACTION_PERMIT, "must PERMIT");
        assert_ne!(
            f.flags & FWPM_FILTER_FLAG_CLEAR_ACTION_RIGHT,
            0,
            "must be a HARD permit (clears action-write right) to beat a GPO block"
        );
        assert_eq!(guid_u128(f.subLayerKey), guid_u128(SUBLAYER_GUID));
        assert_eq!(f.numFilterConditions, 1, "exactly one condition");
        assert_eq!(
            guid_u128(f.layerKey),
            guid_u128(FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4)
        );

        // The single condition is the LUID interface match.
        let c = unsafe { &*f.filterCondition };
        assert_eq!(
            guid_u128(c.fieldKey),
            guid_u128(FWPM_CONDITION_IP_LOCAL_INTERFACE)
        );
        assert_eq!(c.matchType, FWP_MATCH_EQUAL);
        assert_eq!(c.conditionValue.r#type, FWP_UINT64);
        assert_eq!(
            unsafe { *c.conditionValue.Anonymous.uint64 },
            0x0123_4567_89ab_cdef
        );
    }
}
