use bson::{doc, oid::ObjectId, DateTime};
use mongodb::Database;
use roomler2_db::models::{
    Plan, Role, Tenant, TenantMember, TenantSettings,
    role::permissions,
};

use super::base::{BaseDao, DaoError, DaoResult};

pub struct TenantDao {
    pub base: BaseDao<Tenant>,
    pub members: BaseDao<TenantMember>,
    pub roles: BaseDao<Role>,
}

impl TenantDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Tenant::COLLECTION),
            members: BaseDao::new(db, TenantMember::COLLECTION),
            roles: BaseDao::new(db, Role::COLLECTION),
        }
    }

    pub async fn create(
        &self,
        name: String,
        slug: String,
        owner_id: ObjectId,
    ) -> DaoResult<Tenant> {
        let now = DateTime::now();
        let tenant = Tenant {
            id: None,
            name,
            slug,
            description: None,
            icon: None,
            owner_id,
            plan: Plan::Free,
            features: Vec::new(),
            settings: TenantSettings::default(),
            billing: None,
            integrations: None,
            is_archived: false,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        };

        let tenant_id = self.base.insert_one(&tenant).await?;

        // Create default roles
        self.create_default_roles(tenant_id).await?;

        // Add owner as first member
        let owner_role = self.get_role_by_name(tenant_id, "owner").await?;
        self.add_member(tenant_id, owner_id, vec![owner_role.id.unwrap()], None)
            .await?;

        self.base.find_by_id(tenant_id).await
    }

    async fn create_default_roles(&self, tenant_id: ObjectId) -> DaoResult<()> {
        let now = DateTime::now();
        let roles = vec![
            Role {
                id: None,
                tenant_id,
                name: "owner".to_string(),
                description: Some("Full control over the tenant".to_string()),
                color: Some(0xE91E63),
                position: 0,
                permissions: permissions::ALL,
                is_default: false,
                is_managed: true,
                is_mentionable: false,
                is_hoisted: true,
                created_at: now,
                updated_at: now,
            },
            Role {
                id: None,
                tenant_id,
                name: "admin".to_string(),
                description: Some("Administrative access".to_string()),
                color: Some(0x2196F3),
                position: 1,
                permissions: permissions::DEFAULT_ADMIN,
                is_default: false,
                is_managed: true,
                is_mentionable: true,
                is_hoisted: true,
                created_at: now,
                updated_at: now,
            },
            Role {
                id: None,
                tenant_id,
                name: "moderator".to_string(),
                description: Some("Moderate channels and messages".to_string()),
                color: Some(0x4CAF50),
                position: 2,
                permissions: permissions::DEFAULT_MEMBER
                    | permissions::MANAGE_MESSAGES
                    | permissions::MUTE_MEMBERS
                    | permissions::KICK_MEMBERS,
                is_default: false,
                is_managed: true,
                is_mentionable: true,
                is_hoisted: true,
                created_at: now,
                updated_at: now,
            },
            Role {
                id: None,
                tenant_id,
                name: "member".to_string(),
                description: Some("Default member role".to_string()),
                color: None,
                position: 3,
                permissions: permissions::DEFAULT_MEMBER,
                is_default: true,
                is_managed: true,
                is_mentionable: false,
                is_hoisted: false,
                created_at: now,
                updated_at: now,
            },
            Role {
                id: None,
                tenant_id,
                name: "guest".to_string(),
                description: Some("Limited guest access".to_string()),
                color: None,
                position: 4,
                permissions: permissions::VIEW_CHANNELS | permissions::READ_HISTORY,
                is_default: false,
                is_managed: true,
                is_mentionable: false,
                is_hoisted: false,
                created_at: now,
                updated_at: now,
            },
        ];

        for role in &roles {
            self.roles.insert_one(role).await?;
        }
        Ok(())
    }

    pub async fn get_role_by_name(
        &self,
        tenant_id: ObjectId,
        name: &str,
    ) -> DaoResult<Role> {
        self.roles
            .find_one(doc! { "tenant_id": tenant_id, "name": name })
            .await?
            .ok_or(DaoError::NotFound)
    }

    pub async fn add_member(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
        role_ids: Vec<ObjectId>,
        invited_by: Option<ObjectId>,
    ) -> DaoResult<TenantMember> {
        let now = DateTime::now();
        let member = TenantMember {
            id: None,
            tenant_id,
            user_id,
            nickname: None,
            role_ids,
            joined_at: now,
            is_pending: false,
            is_muted: false,
            notification_override: None,
            invited_by,
            last_seen_at: None,
            created_at: now,
            updated_at: now,
        };

        let id = self.members.insert_one(&member).await?;
        self.members.find_by_id(id).await
    }

    pub async fn find_by_slug(&self, slug: &str) -> DaoResult<Tenant> {
        self.base
            .find_one(doc! { "slug": slug, "deleted_at": null })
            .await?
            .ok_or(DaoError::NotFound)
    }

    pub async fn find_user_tenants(&self, user_id: ObjectId) -> DaoResult<Vec<Tenant>> {
        let memberships = self
            .members
            .find_many(doc! { "user_id": user_id }, None)
            .await?;

        let tenant_ids: Vec<ObjectId> = memberships.iter().map(|m| m.tenant_id).collect();

        if tenant_ids.is_empty() {
            return Ok(Vec::new());
        }

        self.base
            .find_many(
                doc! { "_id": { "$in": tenant_ids }, "deleted_at": null },
                Some(doc! { "name": 1 }),
            )
            .await
    }

    pub async fn is_member(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
    ) -> DaoResult<bool> {
        let count = self
            .members
            .count(doc! { "tenant_id": tenant_id, "user_id": user_id })
            .await?;
        Ok(count > 0)
    }

    pub async fn assign_role(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
        role_id: ObjectId,
    ) -> DaoResult<bool> {
        self.members
            .update_one(
                doc! { "tenant_id": tenant_id, "user_id": user_id },
                doc! { "$addToSet": { "role_ids": role_id }, "$set": { "updated_at": DateTime::now() } },
            )
            .await
    }

    pub async fn remove_role(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
        role_id: ObjectId,
    ) -> DaoResult<bool> {
        self.members
            .update_one(
                doc! { "tenant_id": tenant_id, "user_id": user_id },
                doc! { "$pull": { "role_ids": role_id }, "$set": { "updated_at": DateTime::now() } },
            )
            .await
    }

    pub async fn get_member_permissions(
        &self,
        tenant_id: ObjectId,
        user_id: ObjectId,
    ) -> DaoResult<u64> {
        let member = self
            .members
            .find_one(doc! { "tenant_id": tenant_id, "user_id": user_id })
            .await?
            .ok_or(DaoError::Forbidden("Not a member".to_string()))?;

        let roles = self
            .roles
            .find_many(
                doc! { "_id": { "$in": &member.role_ids } },
                None,
            )
            .await?;

        let combined = roles.iter().fold(0u64, |acc, r| acc | r.permissions);
        Ok(combined)
    }
}
