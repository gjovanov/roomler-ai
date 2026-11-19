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

/// How much of a chosen name the person at the machine is told, and how much of a
/// long address. Their sum leaves [`asked_by`]'s own words in view on a consent
/// panel's one line, about 65 characters wide.
const ASKED_NAME_SHOWN: usize = 40;
const ADDRESS_LOCAL_SHOWN: usize = 16;
const ADDRESS_DOMAIN_SHOWN: usize = 28;

/// ⚠️ The format characters `char::is_control` does not cover. A bidi override
/// turns the rest of the line around, and a zero-width character hides in a name.
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// What a stranger's text keeps on its way to someone's screen: one line, bounded,
/// and nothing that can rewrite what stands beside it. Double quotes become single,
/// so it cannot close its own quotation.
fn plain(text: &str, max: usize) -> String {
    let kept: String = text
        .chars()
        .filter(|c| !c.is_control() && !is_format(*c))
        .map(|c| match c {
            '"' | '\u{201C}' | '\u{201D}' => '\'',
            other => other,
        })
        .collect();
    let line = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded: String = line.chars().take(max).collect();
    bounded.trim_end().to_string()
}

/// A name someone chose, fit to quote: one line, bounded, in quotes.
fn quoted(name: &str) -> String {
    format!("\"{}\"", plain(name, NAME_SHOWN))
}

/// An address on one line. A long one is cut at the end of its local part and the
/// front of its domain, which keeps the part worth reading:
/// `it-support.your-...@attacker.example`.
fn shown_address(address: &str) -> String {
    let address = plain(address, usize::MAX);
    let Some((local, domain)) = address.rsplit_once('@') else {
        return plain(&address, ADDRESS_LOCAL_SHOWN + ADDRESS_DOMAIN_SHOWN);
    };
    let local = if local.chars().count() > ADDRESS_LOCAL_SHOWN {
        let head: String = local.chars().take(ADDRESS_LOCAL_SHOWN).collect();
        format!("{head}...")
    } else {
        local.to_string()
    };
    let over = domain.chars().count().saturating_sub(ADDRESS_DOMAIN_SHOWN);
    let domain = if over > 0 {
        let tail: String = domain.chars().skip(over).collect();
        format!("...{tail}")
    } else {
        domain.to_string()
    };
    format!("{local}@{domain}")
}

/// Decision 8 — who the person at the machine is told is asking: the account's
/// proven address, or that it has none, and the name they chose after that.
///
/// ⚠️ The order is the point. Every consent panel draws this on one line and cuts
/// its end, and "Alice from IT" is the classic support scam. The chosen name can
/// neither come first nor push the server's own words out of view, and whatever it
/// says, it says inside quotes, after them.
pub(crate) fn asked_by(address: Option<&str>, name: &str) -> String {
    let name = plain(name, ASKED_NAME_SHOWN);
    match (address.map(shown_address), name.is_empty()) {
        (Some(address), true) => format!("{address} (verified)"),
        (Some(address), false) => format!("{address} (verified) as \"{name}\""),
        (None, true) => "Unverified account".to_string(),
        (None, false) => format!("Unverified account \"{name}\""),
    }
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

    /// The server's words first, and the chosen name in quotes after them, in the
    /// two states an account can be in.
    #[test]
    fn the_prompt_leads_with_what_the_server_knows() {
        assert_eq!(
            asked_by(Some("a@corp.example"), "Alice from IT"),
            "a@corp.example (verified) as \"Alice from IT\""
        );
        assert_eq!(
            asked_by(None, "Alice from IT"),
            "Unverified account \"Alice from IT\""
        );
        assert_eq!(
            asked_by(Some("a@corp.example"), ""),
            "a@corp.example (verified)"
        );
        assert_eq!(asked_by(None, ""), "Unverified account");
        assert_eq!(asked_by(None, " \u{202E}\u{200B} "), "Unverified account");
    }

    /// A name cannot forge the server's words. Written as an address and a claim, it
    /// stays inside its quotes, after the words that say what it is.
    #[test]
    fn a_chosen_name_cannot_forge_the_servers_words() {
        let forged = "ceo@corp.example (verified)";
        assert_eq!(
            asked_by(None, forged),
            "Unverified account \"ceo@corp.example (verified)\""
        );
        // It cannot close its own quotation, in any of the three quote marks.
        for quote in ['"', '\u{201C}', '\u{201D}'] {
            let told = asked_by(None, &format!("x{quote} ceo@corp.example (verified)"));
            assert_eq!(
                told, "Unverified account \"x' ceo@corp.example (verified)\"",
                "{quote}"
            );
        }
        // Nor turn the line around, split it, or hide in it.
        assert_eq!(
            asked_by(None, "x\u{202E}y\u{2066}z\nw\u{200B}v\u{2028}u\u{E0041}t"),
            "Unverified account \"xyzwv ut\""
        );
    }

    /// Nothing a name does can push the server's words past the panel's one line.
    #[test]
    fn a_long_name_or_address_leaves_the_servers_words_in_view() {
        const PANEL: usize = 65;
        let name = "x".repeat(500);
        let told = asked_by(None, &name);
        assert!(told.starts_with("Unverified account \""), "{told}");
        assert!(told.chars().count() <= PANEL, "{told}");

        let told = asked_by(
            Some(&format!("{}@{}", "l".repeat(200), "d".repeat(200))),
            &name,
        );
        let words = told.find(" (verified)").expect("said") + " (verified)".len();
        assert!(told[..words].chars().count() <= PANEL, "{told}");
    }

    /// A long address keeps the part worth reading.
    #[test]
    fn a_long_address_keeps_the_end_of_its_domain() {
        assert_eq!(
            shown_address("it-support.your-company.example@attacker.example"),
            "it-support.your-...@attacker.example"
        );
        assert_eq!(
            shown_address(&format!("a@{}attacker.example", "sub.".repeat(40))),
            "a@...sub.sub.sub.attacker.example"
        );
        assert_eq!(shown_address("a@corp.example"), "a@corp.example");
    }
}
