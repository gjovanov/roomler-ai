// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_db::models::{
    MAX_TUTORIAL_CHAPTER_ID_LEN, MAX_TUTORIAL_CHAPTERS, NotificationPrefs, OAuthProvider, Presence,
    TutorialState, User, UserStatusInfo,
};

use super::base::{BaseDao, DaoError, DaoResult};

/// Domain for addresses that are recorded but **not owned**.
///
/// `.invalid` is reserved by RFC 2606 precisely so it can never be delegated,
/// so nothing here can collide with, or be mistaken for, a real address — and
/// mail to it cannot leave the building.
const UNPROVEN_EMAIL_DOMAIN: &str = "unverified.invalid";

/// Placeholder `email` for an account created from an OAuth identity whose
/// address the provider did not verify.
///
/// Deterministic in the provider identity so the same identity maps to the
/// same row rather than accumulating one per sign-in (step 1 catches it first
/// in practice; this makes the index agree).
fn unverified_placeholder_email(provider: &str, provider_id: &str) -> String {
    let id: String = provider_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("{provider}-{id}@{UNPROVEN_EMAIL_DOMAIN}")
}

/// Placeholder `email` for an account whose unproven claim on a real address
/// was evicted in favour of an identity that proved it. Keyed on the account's
/// own id, which is unique by construction.
fn evicted_placeholder_email(user_id: &ObjectId) -> String {
    format!("unclaimed-{}@{UNPROVEN_EMAIL_DOMAIN}", user_id.to_hex())
}

pub struct UserDao {
    pub base: BaseDao<User>,
}

/// FR-11: what the member grid needs per user, batched.
#[derive(Debug, Clone)]
pub struct MemberFacts {
    pub display_name: String,
    pub username: String,
    pub email: String,
}

impl UserDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, User::COLLECTION),
        }
    }

    pub async fn create(
        &self,
        email: String,
        username: String,
        display_name: String,
        password_hash: String,
    ) -> DaoResult<User> {
        let now = DateTime::now();
        let user = User {
            id: None,
            email,
            unverified_email: None,
            username,
            display_name,
            avatar: None,
            bio: None,
            password_hash: Some(password_hash),
            status: UserStatusInfo::default(),
            presence: Presence::Offline,
            locale: "en-US".to_string(),
            timezone: "UTC".to_string(),
            is_verified: false,
            is_mfa_enabled: false,
            last_active_at: None,
            oauth_providers: Vec::new(),
            notification_preferences: NotificationPrefs::default(),
            tutorial: TutorialState::default(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };

        let id = self.base.insert_one(&user).await?;
        self.base.find_by_id(id).await
    }

    pub async fn find_by_email(&self, email: &str) -> DaoResult<User> {
        self.base
            .find_one(doc! { "email": email, "deleted_at": null })
            .await?
            .ok_or(DaoError::NotFound)
    }

    pub async fn find_by_username(&self, username: &str) -> DaoResult<User> {
        self.base
            .find_one(doc! { "username": username, "deleted_at": null })
            .await?
            .ok_or(DaoError::NotFound)
    }

    pub async fn update_presence(&self, user_id: ObjectId, presence: Presence) -> DaoResult<bool> {
        self.base
            .update_by_id(
                user_id,
                doc! {
                    "$set": {
                        "presence": bson::to_bson(&presence)?,
                        "last_active_at": DateTime::now(),
                    }
                },
            )
            .await
    }

    /// Resolve an OAuth sign-in to a user: by provider identity first,
    /// then — only when the provider PROVED the address
    /// (`email_verified`) — by email, else create a fresh account.
    ///
    /// The gate is the nOAuth fix: Microsoft's multi-tenant endpoint
    /// returns a `mail` attribute any tenant admin can set to an
    /// arbitrary address, so treating it as an account key let a hostile
    /// tenant claim someone else's account. An unverified identity still
    /// signs in — it just gets its own account instead of inheriting one.
    ///
    /// The one invariant everything here serves:
    ///
    /// > **`users.email` holds an address only if that account proved it.**
    ///
    /// It is a unique index, so it is not merely a contact field — it is a
    /// *reservation*, and it is what invites and account-linking key off.
    /// Three consequences, all of which used to be violated:
    ///
    /// - An unverified assertion never lands in `email`; it goes to
    ///   `unverified_email` and the row takes a `.invalid` placeholder
    ///   ([`unverified_placeholder_email`]). Otherwise a hostile tenant could
    ///   reserve an address it does not own, block the real owner's sign-up,
    ///   and collect invites addressed to it.
    /// - Linking (step 2) requires the *target* account to be verified too. A
    ///   one-sided check is not enough: an attacker who registers with a
    ///   password as `victim@corp` and never activates leaves an unproven
    ///   account holding the address, and the victim's later Google sign-in
    ///   would be merged straight into it — password included.
    /// - When a proven identity meets an unproven claim on the same address,
    ///   the claim is **evicted**, not merged (step 2b). Safe by construction:
    ///   an unverified account cannot have been used, because password login
    ///   refuses it and it owns no verified provider identity.
    ///
    /// On top of those three, step 2c refuses an UNVERIFIED identity whose
    /// asserted address already belongs to someone — a UX choice rather than a
    /// security control, since the placeholder already makes that case safe.
    /// See the comment there before touching it.
    pub async fn find_or_create_by_oauth(
        &self,
        provider: &str,
        provider_id: &str,
        email: &str,
        display_name: &str,
        avatar_url: Option<&str>,
        email_verified: bool,
    ) -> DaoResult<User> {
        // 1. Try to find user by OAuth provider + provider_id
        if let Some(user) = self
            .base
            .find_one(doc! {
                "oauth_providers.provider": provider,
                "oauth_providers.provider_id": provider_id,
                "deleted_at": null,
            })
            .await?
        {
            return Ok(user);
        }

        // 2. Resolve by address — ONLY for one the provider proved.
        if email_verified {
            match self.find_by_email(email).await {
                // 2a. Held by an account that proved it: link and sign in.
                Ok(mut user) if user.is_verified => {
                    let already_linked = user
                        .oauth_providers
                        .iter()
                        .any(|p| p.provider == provider && p.provider_id == provider_id);
                    if !already_linked {
                        let oauth = OAuthProvider {
                            provider: provider.to_string(),
                            provider_id: provider_id.to_string(),
                            access_token: None,
                            refresh_token: None,
                        };
                        let oauth_bson = bson::to_bson(&oauth)?;
                        self.base
                            .update_by_id(
                                user.id.unwrap(),
                                doc! { "$push": { "oauth_providers": oauth_bson } },
                            )
                            .await?;
                        user.oauth_providers.push(oauth);
                    }
                    return Ok(user);
                }
                // 2b. Held WITHOUT proof. The arriving identity has proof and
                //     this one does not, so the claim is evicted rather than
                //     inherited, and we fall through to create a clean
                //     account. Nothing is destroyed: an unverified account
                //     cannot have been signed into (password login refuses it,
                //     and any verified provider identity would have made it
                //     verified), so there is no session or content to lose.
                Ok(squatter) => {
                    let holder = squatter.id.unwrap();
                    self.base
                        .update_by_id(
                            holder,
                            doc! { "$set": {
                                "email": evicted_placeholder_email(&holder),
                                "unverified_email": &squatter.email,
                                // update_by_id does not stamp this itself.
                                "updated_at": DateTime::now(),
                            }},
                        )
                        .await?;
                    tracing::warn!(
                        evicted_user = %holder,
                        provider,
                        "oauth: address was held by an unverified account; \
                         evicted the unproven claim in favour of the \
                         provider-verified identity"
                    );
                }
                Err(DaoError::NotFound) => {}
                // A real read failure must NOT be read as "nobody has this
                // address" — that would create a duplicate account for
                // someone who already has one.
                Err(e) => return Err(e),
            }
        }

        // 2c. The address is taken and nothing above claimed the sign-in, so
        //     the provider did not verify it — a verified one would have linked
        //     at 2a or evicted at 2b.
        //
        //     ⚠️ Refusing here is a UX POLICY, not the security control, and the
        //     distinction matters if you ever edit this. It ARRIVED as a
        //     mechanical necessity (#610): the create below used to write the
        //     asserted address, so a second row for one address collided on
        //     EMAIL, and the retry loop — which assumes every duplicate key is a
        //     username clash — burned five attempts and blamed a username the
        //     user never chose. The placeholder at step 3 removed that
        //     collision, so this branch COULD now mint a parallel account and
        //     succeed.
        //
        //     It still refuses, because that account would be invisible to the
        //     person who owns the address: they would sign in, land somewhere
        //     that is not their account, and have no way to tell. An error
        //     naming the reason is the more useful answer.
        //
        //     What stops the takeover is 2a/2b plus the placeholder — NOT this.
        //     Deleting this guard costs UX; deleting either of those costs
        //     security.
        //
        //     (The message confirms an address is registered. That oracle
        //     already exists at /api/auth/register, which 409s on the same
        //     unique index, so this adds no new exposure — but don't extend the
        //     pattern to a surface where it would.)
        if self.find_by_email(email).await.is_ok() {
            return Err(DaoError::DuplicateKey(format!(
                "an account already exists for {email} and {provider} did not \
                 verify the address — sign in with your existing credentials"
            )));
        }

        // 3. Create new user — generate unique username with retry on collision
        let now = DateTime::now();
        let base_username: String = display_name
            .to_lowercase()
            .replace(' ', "_")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();

        // The unproven address never reaches the unique index. Note this also
        // makes the doc comment's promise true when the address is already
        // taken: before, the retry loop below re-rolled only the *username*,
        // so an unverified identity whose asserted address collided burned
        // five inserts and then failed to sign in at all.
        let (stored_email, unverified_email) = if email_verified {
            (email.to_string(), None)
        } else {
            (
                unverified_placeholder_email(provider, provider_id),
                Some(email.to_string()),
            )
        };

        let user_template = |uname: String| User {
            id: None,
            email: stored_email.clone(),
            unverified_email: unverified_email.clone(),
            username: uname,
            display_name: display_name.to_string(),
            avatar: avatar_url.map(|s| s.to_string()),
            bio: None,
            password_hash: None,
            status: UserStatusInfo::default(),
            presence: Presence::Offline,
            locale: "en-US".to_string(),
            timezone: "UTC".to_string(),
            // "This account's address is proven", the same meaning the
            // password path gives it — not "this account may sign in".
            // OAuth sign-in never consulted it and still doesn't.
            is_verified: email_verified,
            is_mfa_enabled: false,
            last_active_at: None,
            oauth_providers: vec![OAuthProvider {
                provider: provider.to_string(),
                provider_id: provider_id.to_string(),
                access_token: None,
                refresh_token: None,
            }],
            notification_preferences: NotificationPrefs::default(),
            tutorial: TutorialState::default(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };

        // Retry up to 5 times with different suffixes on username collision.
        //
        // ⚠️ The retry is only legitimate for a USERNAME collision, because the
        // username is the only thing this loop changes between attempts. Any
        // other duplicate is unresolvable by repetition, and swallowing it here
        // is how a real defect hid: the loop used to accept every
        // `DuplicateKey`, burn all five attempts on an EMAIL collision, and
        // then report "Failed to generate unique username after retries" — an
        // error naming a field that was never the problem. That message made
        // `unverified_oauth_email_never_links_into_an_existing_account` look
        // like fixture noise for two sessions (#613).
        let mut last_duplicate: Option<String> = None;
        for _ in 0..5 {
            let username = format!("{}_{}", base_username, &ObjectId::new().to_hex()[..6]);
            let user = user_template(username);
            match self.base.insert_one(&user).await {
                Ok(id) => return self.base.find_by_id(id).await,
                Err(DaoError::DuplicateKey(msg)) => {
                    if !super::base::duplicate_key_is_on(&msg, "username") {
                        // Not ours to fix by re-rolling — surface Mongo's own
                        // message, which names the colliding field.
                        return Err(DaoError::DuplicateKey(msg));
                    }
                    last_duplicate = Some(msg);
                }
                Err(e) => return Err(e),
            }
        }

        Err(DaoError::DuplicateKey(format!(
            "could not generate a unique username after 5 attempts; last collision: {}",
            last_duplicate.as_deref().unwrap_or("<no detail>")
        )))
    }

    /// Batch-fetch display names for a list of user IDs.
    /// Returns a HashMap mapping ObjectId → display_name (falls back to username).
    pub async fn find_display_names(
        &self,
        user_ids: &[ObjectId],
    ) -> DaoResult<std::collections::HashMap<ObjectId, String>> {
        use futures::TryStreamExt;
        let mut result = std::collections::HashMap::new();
        if user_ids.is_empty() {
            return Ok(result);
        }

        let ids_bson: Vec<bson::Bson> = user_ids
            .iter()
            .map(|id| bson::Bson::ObjectId(*id))
            .collect();
        let filter = doc! {
            "_id": { "$in": ids_bson },
            "deleted_at": null,
        };

        // Use raw Document to avoid deserialization issues with projection
        let projection = doc! { "_id": 1, "display_name": 1, "username": 1 };
        let coll = self.base.collection().clone_with_type::<bson::Document>();
        let mut cursor = coll.find(filter).projection(projection).await?;

        while let Some(doc) = cursor.try_next().await? {
            if let Ok(id) = doc.get_object_id("_id") {
                let display_name = doc.get_str("display_name").unwrap_or("").to_string();
                let username = doc.get_str("username").unwrap_or("").to_string();
                let name = if display_name.is_empty() {
                    username
                } else {
                    display_name
                };
                if !name.is_empty() {
                    result.insert(id, name);
                }
            }
        }
        Ok(result)
    }

    /// FR-11: the member grid's batch — display name (username fallback),
    /// username and EMAIL per user id. Email exposure is deliberate here:
    /// the caller is the tenant-member list, and members of an org see each
    /// other's addresses (that is what the page is for).
    pub async fn find_member_facts(
        &self,
        user_ids: &[ObjectId],
    ) -> DaoResult<std::collections::HashMap<ObjectId, MemberFacts>> {
        use futures::TryStreamExt;
        let mut result = std::collections::HashMap::new();
        if user_ids.is_empty() {
            return Ok(result);
        }
        let ids_bson: Vec<bson::Bson> = user_ids
            .iter()
            .map(|id| bson::Bson::ObjectId(*id))
            .collect();
        let filter = doc! { "_id": { "$in": ids_bson }, "deleted_at": null };
        let projection = doc! { "_id": 1, "display_name": 1, "username": 1, "email": 1 };
        let coll = self.base.collection().clone_with_type::<bson::Document>();
        let mut cursor = coll.find(filter).projection(projection).await?;
        while let Some(doc) = cursor.try_next().await? {
            if let Ok(id) = doc.get_object_id("_id") {
                let display_name = doc.get_str("display_name").unwrap_or("").to_string();
                let username = doc.get_str("username").unwrap_or("").to_string();
                let email = doc.get_str("email").unwrap_or("").to_string();
                let name = if display_name.is_empty() {
                    username.clone()
                } else {
                    display_name
                };
                result.insert(
                    id,
                    MemberFacts {
                        display_name: name,
                        username,
                        email,
                    },
                );
            }
        }
        Ok(result)
    }

    pub async fn update_profile(
        &self,
        user_id: ObjectId,
        display_name: Option<String>,
        bio: Option<String>,
        avatar: Option<String>,
        locale: Option<String>,
        timezone: Option<String>,
    ) -> DaoResult<bool> {
        let mut update = bson::Document::new();
        if let Some(name) = display_name {
            update.insert("display_name", name);
        }
        if let Some(b) = bio {
            update.insert("bio", b);
        }
        if let Some(av) = avatar {
            update.insert("avatar", av);
        }
        if let Some(loc) = locale {
            update.insert("locale", loc);
        }
        if let Some(tz) = timezone {
            update.insert("timezone", tz);
        }

        if update.is_empty() {
            return Ok(false);
        }

        update.insert("updated_at", DateTime::now());

        self.base
            .update_by_id(user_id, doc! { "$set": update })
            .await
    }

    /// FR-12 P3 — write the caller's tutorial state.
    ///
    /// Deliberately NOT folded into `update_profile`: that route is the
    /// user-editable profile (name, bio, avatar, locale), and this is app
    /// state the tutorial owns. Keeping them apart also keeps
    /// `update_profile`'s already-long positional signature from growing.
    ///
    /// `done` REPLACES rather than unions, because the checkboxes can be
    /// un-ticked and a union would make un-ticking impossible to express.
    /// The client seeds itself with the union on first load, so a device
    /// that ticked a chapter offline does not lose it.
    ///
    /// `seen` only ever SETS the timestamp — it is the "do not ambush this
    /// person again" flag, and nothing in normal use should be able to clear
    /// it. Passing `Some(false)` is therefore a no-op, not a reset.
    pub async fn set_tutorial_state(
        &self,
        user_id: ObjectId,
        done: Option<Vec<String>>,
        seen: Option<bool>,
    ) -> DaoResult<bool> {
        let mut update = bson::Document::new();

        if let Some(list) = done {
            // Bound what a client can write onto its own document. Truncating
            // rather than rejecting: this is a checklist, and a request that
            // is merely too long should not fail a user's tutorial.
            let mut clean: Vec<String> = list
                .into_iter()
                .filter(|c| !c.is_empty() && c.len() <= MAX_TUTORIAL_CHAPTER_ID_LEN)
                .collect();
            clean.dedup();
            clean.truncate(MAX_TUTORIAL_CHAPTERS);
            update.insert("tutorial.done", clean);
        }

        if seen == Some(true) {
            update.insert("tutorial.seen_at", DateTime::now());
        }

        if update.is_empty() {
            return Ok(false);
        }

        update.insert("updated_at", DateTime::now());

        self.base
            .update_by_id(user_id, doc! { "$set": update })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the placeholder: it satisfies the unique index
    /// without reserving anything a real person could ever hold.
    #[test]
    fn placeholders_land_in_the_reserved_invalid_tld() {
        let ms = unverified_placeholder_email("microsoft", "00000000-1111-2222-3333-444444444444");
        assert!(
            ms.ends_with("@unverified.invalid"),
            "placeholder must be undeliverable: {ms}"
        );
        let evicted = evicted_placeholder_email(&ObjectId::new());
        assert!(evicted.ends_with("@unverified.invalid"), "{evicted}");
    }

    /// A hostile provider controls `provider_id`. If it could smuggle an `@`
    /// or a domain separator through, it could aim the placeholder at a real
    /// address and re-open exactly the reservation this prevents.
    #[test]
    fn a_hostile_provider_id_cannot_escape_the_placeholder_domain() {
        for hostile in [
            "victim@corp.example",
            "x@corp.example#",
            "a\"b@c",
            "..@..",
            "sub.domain",
        ] {
            let got = unverified_placeholder_email("microsoft", hostile);
            assert_eq!(
                got.matches('@').count(),
                1,
                "exactly one @ separator, got {got}"
            );
            assert!(
                got.ends_with("@unverified.invalid"),
                "must stay in the reserved domain, got {got}"
            );
        }
    }

    /// Deterministic per identity — a re-sign-in must map to the same row
    /// rather than pile up one account per visit.
    #[test]
    fn the_same_identity_always_yields_the_same_placeholder() {
        let a = unverified_placeholder_email("microsoft", "oid-1");
        let b = unverified_placeholder_email("microsoft", "oid-1");
        assert_eq!(a, b);
    }

    /// Distinct identities must not collide, or the second one's insert fails
    /// against the unique index and that user simply cannot sign in.
    #[test]
    fn distinct_identities_do_not_collide() {
        let seen = [
            unverified_placeholder_email("microsoft", "oid-1"),
            unverified_placeholder_email("microsoft", "oid-2"),
            unverified_placeholder_email("github", "oid-1"),
            evicted_placeholder_email(&ObjectId::new()),
            evicted_placeholder_email(&ObjectId::new()),
        ];
        let unique: std::collections::HashSet<&String> = seen.iter().collect();
        assert_eq!(unique.len(), seen.len(), "placeholders collided: {seen:?}");
    }
}
