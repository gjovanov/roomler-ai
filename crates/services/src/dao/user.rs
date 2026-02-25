use bson::{doc, oid::ObjectId, DateTime};
use mongodb::Database;
use roomler2_db::models::{NotificationPrefs, OAuthProvider, Presence, User, UserStatusInfo};

use super::base::{BaseDao, DaoError, DaoResult};

pub struct UserDao {
    pub base: BaseDao<User>,
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

    pub async fn update_presence(
        &self,
        user_id: ObjectId,
        presence: Presence,
    ) -> DaoResult<bool> {
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

    pub async fn find_or_create_by_oauth(
        &self,
        provider: &str,
        provider_id: &str,
        email: &str,
        display_name: &str,
        avatar_url: Option<&str>,
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

        // 2. Try to find user by email and link the OAuth provider
        if let Ok(mut user) = self.find_by_email(email).await {
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
                let oauth_bson =
                    bson::to_bson(&oauth)?;
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

        // 3. Create new user — generate unique username with retry on collision
        let now = DateTime::now();
        let base_username: String = display_name
            .to_lowercase()
            .replace(' ', "_")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();

        let user_template = |uname: String| User {
            id: None,
            email: email.to_string(),
            username: uname,
            display_name: display_name.to_string(),
            avatar: avatar_url.map(|s| s.to_string()),
            bio: None,
            password_hash: None,
            status: UserStatusInfo::default(),
            presence: Presence::Offline,
            locale: "en-US".to_string(),
            timezone: "UTC".to_string(),
            is_verified: true,
            is_mfa_enabled: false,
            last_active_at: None,
            oauth_providers: vec![OAuthProvider {
                provider: provider.to_string(),
                provider_id: provider_id.to_string(),
                access_token: None,
                refresh_token: None,
            }],
            notification_preferences: NotificationPrefs::default(),
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };

        // Retry up to 5 times with different suffixes on username collision
        for _ in 0..5 {
            let username = format!("{}_{}", base_username, &ObjectId::new().to_hex()[..6]);
            let user = user_template(username);
            match self.base.insert_one(&user).await {
                Ok(id) => return self.base.find_by_id(id).await,
                Err(DaoError::DuplicateKey(_)) => continue,
                Err(e) => return Err(e),
            }
        }

        Err(DaoError::DuplicateKey(
            "Failed to generate unique username after retries".to_string(),
        ))
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
}
