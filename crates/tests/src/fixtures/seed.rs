use bson::oid::ObjectId;
use serde_json::Value;

use super::test_app::TestApp;

/// Result of seeding a test tenant with users and rooms.
pub struct SeededTenant {
    pub tenant_id: String,
    pub tenant_slug: String,
    pub admin: SeededUser,
    pub member: SeededUser,
    pub rooms: Vec<SeededRoom>,
}

pub struct SeededUser {
    pub id: String,
    pub email: String,
    pub username: String,
    pub access_token: String,
    pub refresh_token: String,
}

pub struct SeededRoom {
    pub id: String,
    pub name: String,
    pub path: String,
}

impl TestApp {
    /// Register a user and return their auth info.
    pub async fn register_user(
        &self,
        email: &str,
        username: &str,
        display_name: &str,
        password: &str,
        tenant_name: Option<&str>,
        tenant_slug: Option<&str>,
    ) -> SeededUser {
        let mut body = serde_json::json!({
            "email": email,
            "username": username,
            "display_name": display_name,
            "password": password,
        });

        if let (Some(tn), Some(ts)) = (tenant_name, tenant_slug) {
            body["tenant_name"] = serde_json::json!(tn);
            body["tenant_slug"] = serde_json::json!(ts);
        }

        let resp = self
            .client
            .post(self.url("/api/auth/register"))
            .json(&body)
            .send()
            .await
            .expect("Register request failed");

        assert_eq!(
            resp.status().as_u16(),
            201,
            "Register failed: {}",
            resp.text().await.unwrap_or_default()
        );

        // Re-send to get the parsed response (reqwest consumed the body)
        let resp = self
            .client
            .post(self.url("/api/auth/login"))
            .json(&serde_json::json!({
                "email": email,
                "password": password,
            }))
            .send()
            .await
            .expect("Login request failed");

        let json: Value = resp.json().await.expect("Failed to parse login response");

        SeededUser {
            id: json["user"]["id"].as_str().unwrap().to_string(),
            email: email.to_string(),
            username: username.to_string(),
            access_token: json["access_token"].as_str().unwrap().to_string(),
            refresh_token: json["refresh_token"].as_str().unwrap().to_string(),
        }
    }

    /// Login a user and return their auth info.
    pub async fn login_user(&self, email: &str, password: &str) -> SeededUser {
        let resp = self
            .client
            .post(self.url("/api/auth/login"))
            .json(&serde_json::json!({
                "email": email,
                "password": password,
            }))
            .send()
            .await
            .expect("Login request failed");

        assert!(
            resp.status().is_success(),
            "Login failed: {}",
            resp.text().await.unwrap_or_default()
        );

        let json: Value = resp.json().await.expect("Failed to parse login response");

        SeededUser {
            id: json["user"]["id"].as_str().unwrap().to_string(),
            email: email.to_string(),
            username: json["user"]["username"].as_str().unwrap().to_string(),
            access_token: json["access_token"].as_str().unwrap().to_string(),
            refresh_token: json["refresh_token"].as_str().unwrap().to_string(),
        }
    }

    /// Create an authenticated request with the given token.
    pub fn auth_get(&self, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.client
            .get(self.url(path))
            .header("Authorization", format!("Bearer {}", token))
    }

    pub fn auth_post(&self, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.client
            .post(self.url(path))
            .header("Authorization", format!("Bearer {}", token))
    }

    pub fn auth_put(&self, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.client
            .put(self.url(path))
            .header("Authorization", format!("Bearer {}", token))
    }

    pub fn auth_delete(&self, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.client
            .delete(self.url(path))
            .header("Authorization", format!("Bearer {}", token))
    }

    /// Seed a full tenant with admin + member users, and 3 rooms.
    pub async fn seed_tenant(&self, slug: &str) -> SeededTenant {
        let tenant_name = format!("{} Corp", slug);

        // Register admin (creates tenant)
        let admin = self
            .register_user(
                &format!("admin@{}.test", slug),
                &format!("{}_admin", slug),
                &format!("{} Admin", slug),
                "Admin123!",
                Some(&tenant_name),
                Some(slug),
            )
            .await;

        // Get tenant ID
        let resp = self
            .auth_get("/api/tenant", &admin.access_token)
            .send()
            .await
            .expect("List tenants failed");
        let tenants: Vec<Value> = resp.json().await.unwrap();
        let tenant_id = tenants
            .iter()
            .find(|t| t["slug"].as_str() == Some(slug))
            .expect("Tenant not found")["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Register a regular member
        let member = self
            .register_user(
                &format!("member@{}.test", slug),
                &format!("{}_member", slug),
                &format!("{} Member", slug),
                "Member123!",
                None,
                None,
            )
            .await;

        // Add member to tenant (via direct DB for simplicity)
        {
            use bson::doc;
            let tid = ObjectId::parse_str(&tenant_id).unwrap();
            let uid = ObjectId::parse_str(&member.id).unwrap();

            // Get the "member" role
            let role: bson::Document = self
                .db
                .collection::<bson::Document>("roles")
                .find_one(doc! { "tenant_id": tid, "name": "member" })
                .await
                .unwrap()
                .expect("member role not found");
            let role_id = role.get_object_id("_id").unwrap();

            let now = bson::DateTime::now();
            let member_doc = doc! {
                "tenant_id": tid,
                "user_id": uid,
                "nickname": bson::Bson::Null,
                "role_ids": [role_id],
                "joined_at": now,
                "is_pending": false,
                "is_muted": false,
                "notification_override": bson::Bson::Null,
                "invited_by": bson::Bson::Null,
                "last_seen_at": bson::Bson::Null,
                "created_at": now,
                "updated_at": now,
            };
            self.db
                .collection::<bson::Document>("tenant_members")
                .insert_one(member_doc)
                .await
                .expect("Failed to add member to tenant");
        }

        // Create rooms
        let room_names = ["general", "engineering", "random"];
        let mut rooms = Vec::new();

        for name in &room_names {
            let resp = self
                .auth_post(
                    &format!("/api/tenant/{}/room", tenant_id),
                    &admin.access_token,
                )
                .json(&serde_json::json!({
                    "name": name,
                    "is_open": true,
                }))
                .send()
                .await
                .expect("Create room failed");

            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            assert!(
                status.is_success(),
                "Create room '{}' failed (status {}): {}",
                name, status, body
            );

            let json: Value = serde_json::from_str(&body)
                .unwrap_or_else(|e| panic!("Failed to parse room response for '{}': {} body='{}'", name, e, body));
            rooms.push(SeededRoom {
                id: json["id"].as_str().unwrap().to_string(),
                name: name.to_string(),
                path: json["path"].as_str().unwrap().to_string(),
            });
        }

        SeededTenant {
            tenant_id,
            tenant_slug: slug.to_string(),
            admin,
            member,
            rooms,
        }
    }
}
