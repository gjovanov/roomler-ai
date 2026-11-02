// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
//! FR-52 P5 — what an org hears when someone from outside comes near a device.
//!
//! Two notices, each in the app and by web push, to the org's `MANAGE_AGENTS`
//! holders and the device's owner:
//!
//! * **The first session** a person ever opens on a device (`note_session` says
//!   which that is, across pods and past the audit's TTL). Not every session:
//!   the person at the machine sees the prompt for those (decision 2).
//! * **Repeated guessing**: the device has started refusing guesses. A wrong
//!   password leaves nothing else behind (§4b), and there is no lockout
//!   (decision 7), so this is how a human learns of a sustained attack. It says
//!   what to do, because rotating the connect code ends it.
//!
//! Best-effort end to end, like `fleet::pairing_notice`: a notice that could not
//! be sent is logged and never costs anything else.
//!
//! ⚠️ `actor` is a name the outsider chose, and "Alice from IT" is the classic
//! support scam (decision 8). It is quoted, on one line, and worded as a claim.

use bson::oid::ObjectId;
use roomler_ai_db::models::role::permissions;
use roomler_ai_db::models::{NotificationSource, NotificationType};
use roomler_ai_remote_control::models::Agent;
use roomler_core::notify::{NotifyParams, PushTo, notify_users};
use tracing::{info, warn};

use crate::RemoteState;

/// How much of a chosen name a notice repeats.
const NAME_SHOWN: usize = 64;

/// A name someone chose, fit to quote: one line, bounded, in quotes.
fn quoted(name: &str) -> String {
    let line: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(NAME_SHOWN)
        .collect();
    format!("\"{}\"", line.trim())
}

fn device_name(agent: &Agent) -> &str {
    agent.display_name.as_deref().unwrap_or(&agent.name)
}

/// Who hears of it: whoever manages the org's devices, and this device's owner
/// while they are still a member. Never the person it is about, who may hold a
/// role in this org too.
async fn people_to_tell(
    state: &RemoteState,
    agent: &Agent,
    principal: ObjectId,
) -> Result<Vec<ObjectId>, roomler_ai_services::dao::base::DaoError> {
    let tenants = &state.fleet.tenants;
    let mut people = tenants
        .members_with_permission(agent.tenant_id, permissions::MANAGE_AGENTS)
        .await?;
    if !people.contains(&agent.owner_user_id)
        && tenants
            .is_member(agent.tenant_id, agent.owner_user_id)
            .await?
    {
        people.push(agent.owner_user_id);
    }
    people.retain(|id| *id != principal);
    Ok(people)
}

async fn notify(
    state: &RemoteState,
    agent: &Agent,
    device: ObjectId,
    principal: ObjectId,
    title: String,
    body: String,
) {
    let people = match people_to_tell(state, agent, principal).await {
        Ok(people) => people,
        Err(e) => {
            warn!(%e, agent = %device, "extauth: nobody could be told");
            return;
        }
    };
    let params = NotifyParams {
        tenant_id: agent.tenant_id,
        notification_type: NotificationType::ExternalAccess,
        title,
        body,
        link: format!("/tenant/{}/audit/external", agent.tenant_id.to_hex()),
        source: NotificationSource {
            entity_type: "agent".to_string(),
            entity_id: device,
            actor_id: Some(principal),
        },
        ws_type_label: "external_access",
    };
    notify_users(&state.core, &params, &people, PushTo::Everyone).await;
    info!(agent = %device, people = people.len(), "extauth: the org was told");
}

/// The first session this person ever opened on this device.
pub async fn first_session(
    state: &RemoteState,
    agent: &Agent,
    device: ObjectId,
    principal: ObjectId,
    actor: &str,
) {
    let title = format!(
        "Someone from outside your organization reached {}",
        device_name(agent)
    );
    let body = format!(
        "An account calling itself {} proved this device's password and asked for a \
         session, its first here. That name is whatever the account chose. If you do \
         not know them, bar them on the external access page and change the password \
         on the device.",
        quoted(actor)
    );
    notify(state, agent, device, principal, title, body).await;
}

/// The device has started refusing guesses at its password. Returns at once.
pub fn spawn_guessing(
    state: &RemoteState,
    agent: &Agent,
    device: ObjectId,
    principal: ObjectId,
    actor: &str,
) {
    let (state, agent, actor) = (state.clone(), agent.clone(), actor.to_string());
    tokio::spawn(async move {
        let title = format!(
            "Someone keeps guessing the password of {}",
            device_name(&agent)
        );
        let body = format!(
            "The device is refusing them for now. The account trying calls itself {}, \
             and that name is whatever it chose. If you expect nobody, rotate the \
             device's connect code: that ends it.",
            quoted(&actor)
        );
        notify(&state, &agent, device, principal, title, body).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name is a stranger's text in an admin's notification: one line, bounded,
    /// and visibly a quotation.
    #[test]
    fn a_chosen_name_is_quoted_on_one_line_and_bounded() {
        assert_eq!(quoted("Alice from IT"), "\"Alice from IT\"");
        assert_eq!(quoted("two\nlines\t"), "\"twolines\"");
        assert_eq!(quoted(&"x".repeat(500)).chars().count(), NAME_SHOWN + 2);
        assert_eq!(quoted(""), "\"\"");
    }
}
