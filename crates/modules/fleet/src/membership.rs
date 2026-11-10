// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! The enrollment keys a person minted in an org end with their membership.
//!
//! An FR-51 ephemeral key acts for the admin who minted it
//! (`EnrollmentKey::created_by`, the key's `sub`): every device it enrolls is
//! theirs. So when that person stops being a member of the org (an admin
//! removes them, or they leave), the live keys they minted there are revoked
//! the way `DELETE …/agent/enroll-key/{key_id}` revokes one:
//! [`MembershipLifecycle::org_member_removed`], run by the host's member route
//! once the membership row is gone. The key list shows them revoked. Devices
//! the keys enrolled are not touched, and an ephemeral one still ends by its
//! own TTL.
//!
//! Every use of a credential also asks whether its issuer may still enroll
//! devices in the org (`crate::agent::issuer_may_enroll`), so this hook is not
//! what stops a key: it makes the end final, so that a re-add does not bring
//! the key back. A key whose minter only lost `MANAGE_AGENTS` is refused but not
//! revoked, and works again if they regain it. A single-use token lives ten
//! minutes and needs no hook: its redemption refuses it.
//!
//! **This hook never stops the cascade.** A failed write is logged at ERROR
//! (`alert = membership_key_revoke`), and the hook answers `Ok`. The cascade
//! stops on a failing holder because `chat` must not drop the room rows
//! `conference` reads to find a person's calls; a key write touches neither,
//! and stopping would leave the person in a live call and on the room fan-out
//! because of it. A key the hook could not revoke still enrolls nothing while
//! its minter is not a member, and its first refused use revokes it.

use async_trait::async_trait;
use bson::oid::ObjectId;
use roomler_core::MembershipLifecycle;
use tracing::{error, warn};

use crate::FleetState;

/// The `alert` field of every log line about an enrollment key that a
/// membership's end, or a use refused for it, could not revoke.
pub(crate) const ALERT: &str = "membership_key_revoke";

/// Fleet's [`MembershipLifecycle`]: the hook `Module::hooks` registers.
pub struct FleetMembershipHooks {
    pub state: FleetState,
}

#[async_trait]
impl MembershipLifecycle for FleetMembershipHooks {
    async fn org_member_removed(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
    ) -> anyhow::Result<()> {
        match self
            .state
            .enrollment_keys
            .revoke_minted_by(tenant_id, user_id, None)
            .await
        {
            Ok(0) => {}
            Ok(revoked) => warn!(
                tenant = %tenant_id, user = %user_id, revoked, reason = "member_removed",
                "fr-51: ephemeral enrollment keys revoked with the org membership"
            ),
            Err(e) => error!(
                tenant = %tenant_id, user = %user_id, %e, alert = ALERT,
                "fr-51: could not revoke the enrollment keys of a member who left the org"
            ),
        }
        // Never `Err`: see the module doc.
        Ok(())
    }
}
