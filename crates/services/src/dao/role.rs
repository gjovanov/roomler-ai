// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (C) 2026 G ROX EOOD
use bson::{DateTime, doc, oid::ObjectId};
use mongodb::Database;
use roomler_ai_db::models::{Role, TenantMember};

use super::base::{BaseDao, DaoError, DaoResult};

pub struct RoleDao {
    pub base: BaseDao<Role>,
    /// The membership rows a deleted role's id is pulled from
    /// ([`Self::delete`]).
    members: BaseDao<TenantMember>,
}

impl RoleDao {
    pub fn new(db: &Database) -> Self {
        Self {
            base: BaseDao::new(db, Role::COLLECTION),
            members: BaseDao::new(db, TenantMember::COLLECTION),
        }
    }

    #[allow(clippy::too_many_arguments)]
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

    #[allow(clippy::too_many_arguments)]
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
        if let Some(n) = name {
            set_doc.insert("name", n);
        }
        if let Some(d) = description {
            set_doc.insert("description", d);
        }
        if let Some(c) = color {
            set_doc.insert("color", c as i64);
        }
        if let Some(p) = perms {
            set_doc.insert("permissions", p as i64);
        }
        if let Some(pos) = position {
            set_doc.insert("position", pos as i64);
        }

        self.base
            .update_one(
                doc! { "_id": role_id, "tenant_id": tenant_id },
                doc! { "$set": set_doc },
            )
            .await
    }

    /// Delete one of `tenant_id`'s roles, and take it from everyone who holds
    /// it there.
    ///
    /// The role is looked up IN the org, so another org's role is a 404 like
    /// no role at all. The row goes first, then the id is pulled from the
    /// org's membership rows. The order is the safe one: once the row is gone
    /// the id counts nowhere, because every reader keeps only the org's live
    /// roles (`TenantDao::member_live_role_ids`), so a pull that fails (logged)
    /// or a concurrent assign that lands after it leaves an id that grants
    /// nothing and matches no rule. Rules and policies that name the role are
    /// left as they are, on purpose: an empty role allow-list means "no
    /// restriction by role", so pruning the last id would widen them. They
    /// keep the id, and it matches nobody.
    pub async fn delete(&self, role_id: ObjectId, tenant_id: ObjectId) -> DaoResult<bool> {
        // Prevent deleting default/managed roles
        let role = self.base.find_by_id_in_tenant(tenant_id, role_id).await?;
        if role.is_default || role.is_managed {
            return Err(DaoError::Forbidden(
                "Cannot delete default or managed roles".into(),
            ));
        }

        let result = self
            .base
            .collection()
            .delete_one(doc! { "_id": role_id, "tenant_id": tenant_id })
            .await?;
        let deleted = result.deleted_count > 0;
        if deleted {
            match self
                .members
                .update_many(
                    doc! { "tenant_id": tenant_id, "role_ids": role_id },
                    doc! { "$pull": { "role_ids": role_id } },
                )
                .await
            {
                Ok(n) => tracing::info!(%tenant_id, %role_id, holders = n,
                    "role deleted and taken from its holders"),
                Err(e) => tracing::warn!(%tenant_id, %role_id, %e,
                    "role deleted, but its id could not be pulled from the members' rows; \
                     it counts nowhere (roles are read live)"),
            }
        }
        Ok(deleted)
    }

    // FR-82 — `seed_defaults` lived here: a SECOND set of managed-role
    // definitions, Titlecased, with no `guest`, and with a `Moderator` that
    // carried `MANAGE_MEETINGS` and not `REMOTE_CONTROL` — the reverse of the
    // copy that actually ran (`TenantDao::create_default_roles`). It had no
    // callers, so the divergence was invisible and would have shipped a
    // different org the day someone wired it up. The definitions now live
    // once, in `roomler_ai_db::models::role::MANAGED_ROLES`.
}
