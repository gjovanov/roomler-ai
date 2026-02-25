use bson::{doc, oid::ObjectId, DateTime};
use mongodb::Database;
use roomler2_db::models::Role;
use roomler2_db::models::role::permissions;

use super::base::{BaseDao, DaoError, DaoResult};

pub struct RoleDao {
    pub base: BaseDao<Role>,
}

impl RoleDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Role::COLLECTION),
        }
    }

    pub async fn create(
        &self,
        tenant_id: ObjectId,
        name: String,
        description: Option<String>,
        color: Option<u32>,
        perms: u64,
        is_default: bool,
        is_managed: bool,
        position: u32,
    ) -> DaoResult<Role> {
        let now = DateTime::now();
        let role = Role {
            id: None,
            tenant_id,
            name,
            description,
            color,
            position,
            permissions: perms,
            is_default,
            is_managed,
            is_mentionable: true,
            is_hoisted: false,
            created_at: now,
            updated_at: now,
        };
        let id = self.base.insert_one(&role).await?;
        self.base.find_by_id(id).await
    }

    pub async fn find_for_tenant(&self, tenant_id: ObjectId) -> DaoResult<Vec<Role>> {
        self.base
            .find_many(
                doc! { "tenant_id": tenant_id },
                Some(doc! { "position": 1 }),
            )
            .await
    }

    pub async fn update(
        &self,
        role_id: ObjectId,
        tenant_id: ObjectId,
        name: Option<String>,
        description: Option<String>,
        color: Option<u32>,
        perms: Option<u64>,
        position: Option<u32>,
    ) -> DaoResult<bool> {
        let mut set_doc = doc! { "updated_at": DateTime::now() };
        if let Some(n) = name { set_doc.insert("name", n); }
        if let Some(d) = description { set_doc.insert("description", d); }
        if let Some(c) = color { set_doc.insert("color", c as i64); }
        if let Some(p) = perms { set_doc.insert("permissions", p as i64); }
        if let Some(pos) = position { set_doc.insert("position", pos as i64); }

        self.base
            .update_one(
                doc! { "_id": role_id, "tenant_id": tenant_id },
                doc! { "$set": set_doc },
            )
            .await
    }

    pub async fn delete(
        &self,
        role_id: ObjectId,
        tenant_id: ObjectId,
    ) -> DaoResult<bool> {
        // Prevent deleting default/managed roles
        let role = self.base.find_by_id(role_id).await?;
        if role.is_default || role.is_managed {
            return Err(DaoError::Forbidden("Cannot delete default or managed roles".into()));
        }

        let result = self
            .base
            .collection()
            .delete_one(doc! { "_id": role_id, "tenant_id": tenant_id })
            .await?;
        Ok(result.deleted_count > 0)
    }

    /// Seed default roles for a new tenant. Returns the created roles.
    pub async fn seed_defaults(&self, tenant_id: ObjectId) -> DaoResult<Vec<Role>> {
        let defaults = [
            ("Owner", permissions::ALL, 0u32),
            ("Admin", permissions::DEFAULT_ADMIN, 1),
            ("Moderator",
                permissions::VIEW_CHANNELS
                | permissions::SEND_MESSAGES
                | permissions::SEND_THREADS
                | permissions::EMBED_LINKS
                | permissions::ATTACH_FILES
                | permissions::READ_HISTORY
                | permissions::ADD_REACTIONS
                | permissions::CONNECT_VOICE
                | permissions::SPEAK
                | permissions::STREAM_VIDEO
                | permissions::MANAGE_MESSAGES
                | permissions::KICK_MEMBERS
                | permissions::MUTE_MEMBERS
                | permissions::MANAGE_MEETINGS,
                2,
            ),
            ("Member", permissions::DEFAULT_MEMBER, 3),
        ];

        let mut roles = Vec::new();
        for (name, perms, pos) in defaults {
            let role = self
                .create(tenant_id, name.into(), None, None, perms, true, true, pos)
                .await?;
            roles.push(role);
        }
        Ok(roles)
    }
}
