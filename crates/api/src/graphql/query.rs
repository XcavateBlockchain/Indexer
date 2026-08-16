//! `QueryRoot`: the old SubQuery-shaped read surface (task-5-brief.md) over Task 2's views.
//! Every connection resolver clamps `first`/`offset` via [`crate::guards`] before touching the
//! database -- the depth/complexity guard lives one layer up, in [`crate::router`], because it
//! has to run before juniper parses the query at all.

use carbon_core::graphql::primitives::I64;
use juniper::{graphql_object, FieldError, FieldResult, Value, ID};

use super::context::GraphQLContext;
use super::enums::{unknown_enum_value, ActionType, Permission, RemovalKind, Role};
use super::types::{
    AccessCheck, Admin, AdminConnection, Config, RoleAssignment, RoleAssignmentConnection,
    SyncStatus, WhitelistAction, WhitelistActionConnection,
};
use crate::guards::{clamp_first, clamp_offset};

pub struct QueryRoot;

fn b58(bytes: &[u8]) -> String {
    bs58::encode(bytes).into_string()
}

/// `count(*)` comes back as `i64`; connection `totalCount` is `Int!` (`i32`) per the brief, to
/// stay close to the old schema. This program's row counts are nowhere near `i32::MAX`; a
/// saturating cast is a documented, harmless fallback rather than a panic if that ever changes.
fn total_count_i32(count: i64) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX)
}

/// `NULL` maps to `None` (the action type simply doesn't carry this field); a non-NULL value
/// that doesn't match any known spelling is a data-integrity error, not a silent `None`.
fn parse_opt_enum<T>(
    value: Option<&str>,
    parse: impl FnOnce(&str) -> Option<T>,
    column: &'static str,
) -> FieldResult<Option<T>> {
    match value {
        None => Ok(None),
        Some(v) => parse(v)
            .map(Some)
            .ok_or_else(|| unknown_enum_value(column, v)),
    }
}

fn missing_column(view: &str, column: &str) -> FieldError {
    FieldError::new(
        format!(
            "{view}.{column} was NULL for a row that should always have it (data integrity issue)"
        ),
        Value::null(),
    )
}

#[graphql_object(context = GraphQLContext)]
impl QueryRoot {
    /// Singleton config row. `null` before `initialize_config` has ever been indexed.
    async fn config(context: &GraphQLContext) -> FieldResult<Option<Config>> {
        let row = sqlx::query!(
            r#"SELECT authority AS "authority!", pending_authority, updated_at_slot, updated_at,
                      updated_in_tx
               FROM config_view"#
        )
        .fetch_optional(&context.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let updated_at_slot = row
            .updated_at_slot
            .ok_or_else(|| missing_column("config_view", "updated_at_slot"))?;
        let updated_at = row
            .updated_at
            .ok_or_else(|| missing_column("config_view", "updated_at"))?;
        let updated_in_tx = row
            .updated_in_tx
            .ok_or_else(|| missing_column("config_view", "updated_in_tx"))?;

        Ok(Some(Config {
            id: ID::new("config"),
            authority: b58(&row.authority),
            pending_authority: row.pending_authority.as_deref().map(b58),
            updated_at_slot: I64(updated_at_slot),
            updated_at,
            updated_in_tx,
        }))
    }

    /// `active` filters; omit for both active and removed admins.
    async fn admins(
        context: &GraphQLContext,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<AdminConnection> {
        let limit = clamp_first(first);
        let skip = clamp_offset(offset);

        let rows = sqlx::query!(
            r#"
            SELECT id AS "id!", active AS "active!", added_by AS "added_by!",
                   added_at_slot AS "added_at_slot!", added_at AS "added_at!",
                   added_in_tx AS "added_in_tx!",
                   removed_at_slot, removed_at, removed_in_tx
            FROM admins_view
            WHERE ($1::bool IS NULL OR active = $1)
            ORDER BY added_at_slot DESC, id ASC
            LIMIT $2 OFFSET $3
            "#,
            active,
            limit,
            skip,
        )
        .fetch_all(&context.pool)
        .await?;

        let total = sqlx::query_scalar!(
            r#"SELECT count(*) FROM admins_view WHERE ($1::bool IS NULL OR active = $1)"#,
            active,
        )
        .fetch_one(&context.pool)
        .await?
        .unwrap_or(0);

        let nodes = rows
            .into_iter()
            .map(|r| Admin {
                id: ID::new(r.id),
                active: r.active,
                added_by: r.added_by,
                added_at_slot: I64(r.added_at_slot),
                added_at: r.added_at,
                added_in_tx: r.added_in_tx,
                removed_at_slot: r.removed_at_slot.map(I64),
                removed_at: r.removed_at,
                removed_in_tx: r.removed_in_tx,
            })
            .collect();

        Ok(AdminConnection {
            nodes,
            total_count: total_count_i32(total),
        })
    }

    /// Single `(user, role)` lookup. Soft-deleted (inactive) assignments are returned too --
    /// mirrors the old grpc-api's `GetRoleAssignment` semantic.
    async fn role_assignment(
        context: &GraphQLContext,
        user: String,
        role: Role,
    ) -> FieldResult<Option<RoleAssignment>> {
        role_assignment_row(&context.pool, &user, role).await
    }

    async fn role_assignments(
        context: &GraphQLContext,
        user: Option<String>,
        role: Option<Role>,
        permission: Option<Permission>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<RoleAssignmentConnection> {
        let limit = clamp_first(first);
        let skip = clamp_offset(offset);
        let role_str = role.map(Role::as_db_str);
        let permission_str = permission.map(Permission::as_db_str);

        let rows = sqlx::query!(
            r#"
            SELECT id AS "id!", user_pubkey AS "user_pubkey!", role AS "role!",
                   permission AS "permission!", active AS "active!",
                   rent_payer AS "rent_payer!", assigned_by AS "assigned_by!",
                   assigned_at_slot AS "assigned_at_slot!", assigned_at AS "assigned_at!",
                   assigned_in_tx AS "assigned_in_tx!",
                   updated_at_slot, updated_at,
                   removed_at_slot, removed_at, removed_in_tx, removal_kind, removed_by
            FROM role_assignments_view
            WHERE ($1::text IS NULL OR user_pubkey = $1)
              AND ($2::text IS NULL OR role = $2)
              AND ($3::text IS NULL OR permission = $3)
              AND ($4::bool IS NULL OR active = $4)
            ORDER BY assigned_at_slot DESC, id ASC
            LIMIT $5 OFFSET $6
            "#,
            user,
            role_str,
            permission_str,
            active,
            limit,
            skip,
        )
        .fetch_all(&context.pool)
        .await?;

        let total = sqlx::query_scalar!(
            r#"
            SELECT count(*) FROM role_assignments_view
            WHERE ($1::text IS NULL OR user_pubkey = $1)
              AND ($2::text IS NULL OR role = $2)
              AND ($3::text IS NULL OR permission = $3)
              AND ($4::bool IS NULL OR active = $4)
            "#,
            user,
            role_str,
            permission_str,
            active,
        )
        .fetch_one(&context.pool)
        .await?
        .unwrap_or(0);

        let mut nodes = Vec::with_capacity(rows.len());
        for r in rows {
            nodes.push(role_assignment_from_row(
                r.id,
                r.user_pubkey,
                r.role,
                r.permission,
                r.active,
                r.rent_payer,
                r.assigned_by,
                r.assigned_at_slot,
                r.assigned_at,
                r.assigned_in_tx,
                r.updated_at_slot,
                r.updated_at,
                r.removed_at_slot,
                r.removed_at,
                r.removed_in_tx,
                r.removal_kind,
                r.removed_by,
            )?);
        }

        Ok(RoleAssignmentConnection {
            nodes,
            total_count: total_count_i32(total),
        })
    }

    async fn whitelist_actions(
        context: &GraphQLContext,
        subject: Option<String>,
        actor: Option<String>,
        #[graphql(name = "type")] action_type: Option<ActionType>,
        tx_signature: Option<String>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<WhitelistActionConnection> {
        let limit = clamp_first(first);
        let skip = clamp_offset(offset);
        let type_str = action_type.map(ActionType::as_db_str);

        let rows = sqlx::query!(
            r#"
            SELECT id, type, subject, role, permission, actor, slot, block_time, tx_signature,
                   instruction_index
            FROM whitelist_actions
            WHERE ($1::text IS NULL OR subject = $1)
              AND ($2::text IS NULL OR actor = $2)
              AND ($3::text IS NULL OR type = $3)
              AND ($4::text IS NULL OR tx_signature = $4)
            ORDER BY slot DESC, tx_signature ASC,
                     string_to_array(instruction_index, '.')::int[] ASC, id ASC
            LIMIT $5 OFFSET $6
            "#,
            subject,
            actor,
            type_str,
            tx_signature,
            limit,
            skip,
        )
        .fetch_all(&context.pool)
        .await?;

        let total = sqlx::query_scalar!(
            r#"
            SELECT count(*) FROM whitelist_actions
            WHERE ($1::text IS NULL OR subject = $1)
              AND ($2::text IS NULL OR actor = $2)
              AND ($3::text IS NULL OR type = $3)
              AND ($4::text IS NULL OR tx_signature = $4)
            "#,
            subject,
            actor,
            type_str,
            tx_signature,
        )
        .fetch_one(&context.pool)
        .await?
        .unwrap_or(0);

        let mut nodes = Vec::with_capacity(rows.len());
        for r in rows {
            nodes.push(WhitelistAction {
                id: ID::new(r.id),
                action_type: ActionType::from_db_str(&r.r#type)
                    .ok_or_else(|| unknown_enum_value("whitelist_actions.type", &r.r#type))?,
                subject: r.subject,
                role: parse_opt_enum(
                    r.role.as_deref(),
                    Role::from_db_str,
                    "whitelist_actions.role",
                )?,
                permission: parse_opt_enum(
                    r.permission.as_deref(),
                    Permission::from_db_str,
                    "whitelist_actions.permission",
                )?,
                actor: r.actor,
                slot: I64(r.slot),
                block_time: r.block_time,
                tx_signature: r.tx_signature,
                instruction_index: r.instruction_index,
            });
        }

        Ok(WhitelistActionConnection {
            nodes,
            total_count: total_count_i32(total),
        })
    }

    /// `hasRole` = an active `(user, role)` assignment exists; `compliant` = `hasRole` AND
    /// `permission == COMPLIANT` (ruling R17).
    async fn check_access(
        context: &GraphQLContext,
        user: String,
        role: Role,
    ) -> FieldResult<AccessCheck> {
        let row = role_assignment_row(&context.pool, &user, role).await?;
        Ok(match row {
            Some(a) if a.active => AccessCheck {
                has_role: true,
                compliant: matches!(a.permission, Permission::Compliant),
            },
            _ => AccessCheck {
                has_role: false,
                compliant: false,
            },
        })
    }

    async fn sync_status(context: &GraphQLContext) -> FieldResult<SyncStatus> {
        let state = sqlx::query!(
            r#"SELECT last_contiguous_slot, backfill_complete, snapshot_slot FROM sync_state WHERE id = 1"#
        )
        .fetch_optional(&context.pool)
        .await?
        .ok_or_else(|| FieldError::new("sync_state has not been initialised yet", Value::null()))?;

        let tip = context.chain_tip.get().await?;
        let lag = tip.saturating_sub(state.last_contiguous_slot.max(0) as u64);

        Ok(SyncStatus {
            last_contiguous_slot: I64(state.last_contiguous_slot),
            backfill_complete: state.backfill_complete,
            snapshot_slot: state.snapshot_slot.map(I64),
            chain_tip_slot: I64(tip as i64),
            slot_lag: I64(lag as i64),
        })
    }
}

async fn role_assignment_row(
    pool: &sqlx::PgPool,
    user: &str,
    role: Role,
) -> FieldResult<Option<RoleAssignment>> {
    let role_str = role.as_db_str();
    let row = sqlx::query!(
        r#"
        SELECT id AS "id!", user_pubkey AS "user_pubkey!", role AS "role!",
               permission AS "permission!", active AS "active!",
               rent_payer AS "rent_payer!", assigned_by AS "assigned_by!",
               assigned_at_slot AS "assigned_at_slot!", assigned_at AS "assigned_at!",
               assigned_in_tx AS "assigned_in_tx!",
               updated_at_slot, updated_at,
               removed_at_slot, removed_at, removed_in_tx, removal_kind, removed_by
        FROM role_assignments_view
        WHERE user_pubkey = $1 AND role = $2
        LIMIT 1
        "#,
        user,
        role_str,
    )
    .fetch_optional(pool)
    .await?;

    let Some(r) = row else {
        return Ok(None);
    };
    role_assignment_from_row(
        r.id,
        r.user_pubkey,
        r.role,
        r.permission,
        r.active,
        r.rent_payer,
        r.assigned_by,
        r.assigned_at_slot,
        r.assigned_at,
        r.assigned_in_tx,
        r.updated_at_slot,
        r.updated_at,
        r.removed_at_slot,
        r.removed_at,
        r.removed_in_tx,
        r.removal_kind,
        r.removed_by,
    )
    .map(Some)
}

#[allow(clippy::too_many_arguments)]
fn role_assignment_from_row(
    id: String,
    user_pubkey: String,
    role: String,
    permission: String,
    active: bool,
    rent_payer: String,
    assigned_by: String,
    assigned_at_slot: i64,
    assigned_at: chrono::DateTime<chrono::Utc>,
    assigned_in_tx: String,
    updated_at_slot: Option<i64>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    removed_at_slot: Option<i64>,
    removed_at: Option<chrono::DateTime<chrono::Utc>>,
    removed_in_tx: Option<String>,
    removal_kind: Option<String>,
    removed_by: Option<String>,
) -> FieldResult<RoleAssignment> {
    let role = Role::from_db_str(&role)
        .ok_or_else(|| unknown_enum_value("role_assignments_view.role", &role))?;
    let permission = Permission::from_db_str(&permission)
        .ok_or_else(|| unknown_enum_value("role_assignments_view.permission", &permission))?;
    let removal_kind = parse_opt_enum(
        removal_kind.as_deref(),
        RemovalKind::from_db_str,
        "role_assignments_view.removal_kind",
    )?;
    let updated_at_slot = updated_at_slot
        .ok_or_else(|| missing_column("role_assignments_view", "updated_at_slot"))?;
    let updated_at =
        updated_at.ok_or_else(|| missing_column("role_assignments_view", "updated_at"))?;

    Ok(RoleAssignment {
        id: ID::new(id),
        user: user_pubkey,
        role,
        permission,
        active,
        rent_payer,
        assigned_by,
        assigned_at_slot: I64(assigned_at_slot),
        assigned_at,
        assigned_in_tx,
        updated_at_slot: I64(updated_at_slot),
        updated_at,
        removed_at_slot: removed_at_slot.map(I64),
        removed_at,
        removed_in_tx,
        removal_kind,
        removed_by,
    })
}
