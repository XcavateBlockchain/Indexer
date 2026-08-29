//! `QueryRoot`: the old SubQuery-shaped read surface (task-5-brief.md) over Task 2's views.
//! Every connection resolver clamps `first`/`offset` via [`crate::guards`] before touching the
//! database -- the depth/complexity guard lives one layer up, in [`crate::router`], because it
//! has to run before juniper parses the query at all.

use carbon_core::graphql::primitives::I64;
use juniper::{graphql_object, FieldError, FieldResult, Value, ID};

use super::context::GraphQLContext;
use super::enums::{
    unknown_enum_value, ActionType, ListingStatus, Permission, ProgramName, RegionStatus,
    RemovalKind, Role,
};
use super::programs;
use super::types::{
    AccessCheck, Admin, AdminConnection, Config, ProgramInstruction, ProgramInstructionConnection,
    ProgramSyncStatus, ProgramUpgrade, RoleAssignment, RoleAssignmentConnection, SyncStatus,
    WhitelistAction, WhitelistActionConnection,
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

    /// Fleet aggregates plus per-program rows -- see [`SyncStatus`]'s doc for the aggregate
    /// semantics (min / AND across programs: the stack is only as caught-up as its laggiest
    /// program). Scoped by the api's optional `PROGRAMS` env so subset operation (which
    /// freezes excluded programs' rows) does not drag the aggregates.
    async fn sync_status(context: &GraphQLContext) -> FieldResult<SyncStatus> {
        let rows = sqlx::query!(
            r#"
            SELECT program_id, last_contiguous_slot, backfill_complete, backfill_floor_slot,
                   snapshot_slot
            FROM sync_state
            WHERE ($1::bytea[] IS NULL OR program_id = ANY($1))
            "#,
            context.program_filter.as_deref(),
        )
        .fetch_all(&context.pool)
        .await?;
        if rows.is_empty() {
            return Err(FieldError::new(
                "sync_state has not been initialised yet",
                Value::null(),
            ));
        }

        let last_contiguous_slot = rows
            .iter()
            .map(|r| r.last_contiguous_slot)
            .min()
            .unwrap_or(0);
        let backfill_complete = rows.iter().all(|r| r.backfill_complete);
        // The oldest snapshot; None if any program has never been snapshotted.
        let snapshot_slot = rows
            .iter()
            .map(|r| r.snapshot_slot)
            .collect::<Option<Vec<i64>>>()
            .and_then(|slots| slots.into_iter().min());

        let mut programs = Vec::with_capacity(rows.len());
        for r in &rows {
            // A row for an address outside the compiled-in set would mean the DB and this
            // binary disagree about the program roster -- surface it rather than hiding it.
            let program = ProgramName::from_program_id_bytes(&r.program_id).ok_or_else(|| {
                FieldError::new(
                    format!(
                        "sync_state has a row for an unknown program id: {}",
                        bs58::encode(&r.program_id).into_string()
                    ),
                    Value::null(),
                )
            })?;
            programs.push(ProgramSyncStatus {
                program,
                last_contiguous_slot: I64(r.last_contiguous_slot),
                backfill_complete: r.backfill_complete,
                backfill_floor_slot: I64(r.backfill_floor_slot),
                snapshot_slot: r.snapshot_slot.map(I64),
            });
        }

        let tip = context.chain_tip.get().await?;
        let lag = tip.saturating_sub(last_contiguous_slot.max(0) as u64);

        Ok(SyncStatus {
            last_contiguous_slot: I64(last_contiguous_slot),
            backfill_complete,
            snapshot_slot: snapshot_slot.map(I64),
            chain_tip_slot: I64(tip as i64),
            slot_lag: I64(lag as i64),
            programs,
        })
    }

    /// Each program's recorded version timeline (`program_upgrades`, ADR-24), oldest boundary
    /// first: the seeded deploy slot plus every BPFLoaderUpgradeable upgrade the indexer has
    /// observed on-chain. More than one entry for a program means its bytecode changed after
    /// the checked-in IDL's snapshot -- the signal the maintenance loop acts on (RUNBOOK.md
    /// "After a program upgrade"). Unpaginated on purpose: upgrades are rare, operator-driven
    /// events (a handful of rows per program, ever). Scoped by the api's optional `PROGRAMS`
    /// env like `syncStatus`.
    async fn program_upgrades(
        context: &GraphQLContext,
        program: Option<ProgramName>,
    ) -> FieldResult<Vec<ProgramUpgrade>> {
        let program_id = program.map(|p| p.as_program_id_bytes());
        let rows = sqlx::query!(
            r#"
            SELECT program_id, upgrade_slot, signature, source, detected_at
            FROM program_upgrades
            WHERE ($1::bytea IS NULL OR program_id = $1)
              AND ($2::bytea[] IS NULL OR program_id = ANY($2))
            ORDER BY upgrade_slot ASC
            "#,
            program_id.as_deref(),
            context.program_filter.as_deref(),
        )
        .fetch_all(&context.pool)
        .await?;

        let mut upgrades = Vec::with_capacity(rows.len());
        for r in rows {
            // Same roster hard-fail as syncStatus: a row for an address outside the
            // compiled-in set means the DB and this binary disagree about the programs.
            let program = ProgramName::from_program_id_bytes(&r.program_id).ok_or_else(|| {
                FieldError::new(
                    format!(
                        "program_upgrades has a row for an unknown program id: {}",
                        bs58::encode(&r.program_id).into_string()
                    ),
                    Value::null(),
                )
            })?;
            upgrades.push(ProgramUpgrade {
                program,
                upgrade_slot: I64(r.upgrade_slot),
                signature: r.signature,
                source: r.source,
                detected_at: r.detected_at,
            });
        }
        Ok(upgrades)
    }

    /// The shared instruction history: every successfully indexed instruction of every
    /// indexed program, newest first, with its decoded args as JSON. `txSignature` filters to
    /// one transaction (base58).
    async fn program_instructions(
        context: &GraphQLContext,
        program: Option<ProgramName>,
        ix_name: Option<String>,
        tx_signature: Option<String>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<ProgramInstructionConnection> {
        let limit = clamp_first(first);
        let skip = clamp_offset(offset);
        let program_id = program.map(|p| p.as_program_id_bytes());
        let signature = tx_signature
            .as_deref()
            .map(|s| {
                let bytes = bs58::decode(s).into_vec().map_err(|e| {
                    FieldError::new(format!("txSignature: invalid base58: {e}"), Value::null())
                })?;
                if bytes.len() != 64 {
                    return Err(FieldError::new(
                        format!(
                            "txSignature: expected a 64-byte signature, got {} bytes",
                            bytes.len()
                        ),
                        Value::null(),
                    ));
                }
                Ok(bytes)
            })
            .transpose()?;

        let rows = sqlx::query!(
            r#"
            SELECT program_id, signature, ix_index, inner_index, slot, block_time, ix_name,
                   accounts, data
            FROM program_instructions
            WHERE ($1::bytea IS NULL OR program_id = $1)
              AND ($2::text IS NULL OR ix_name = $2)
              AND ($3::bytea IS NULL OR signature = $3)
            ORDER BY slot DESC, signature ASC, ix_index ASC, inner_index ASC
            LIMIT $4 OFFSET $5
            "#,
            program_id.as_deref(),
            ix_name.as_deref(),
            signature.as_deref(),
            limit,
            skip,
        )
        .fetch_all(&context.pool)
        .await?;

        let total = sqlx::query_scalar!(
            r#"
            SELECT count(*) FROM program_instructions
            WHERE ($1::bytea IS NULL OR program_id = $1)
              AND ($2::text IS NULL OR ix_name = $2)
              AND ($3::bytea IS NULL OR signature = $3)
            "#,
            program_id.as_deref(),
            ix_name.as_deref(),
            signature.as_deref(),
        )
        .fetch_one(&context.pool)
        .await?
        .unwrap_or(0);

        let mut nodes = Vec::with_capacity(rows.len());
        for r in rows {
            let program = ProgramName::from_program_id_bytes(&r.program_id).ok_or_else(|| {
                FieldError::new(
                    format!(
                        "program_instructions has a row for an unknown program id: {}",
                        bs58::encode(&r.program_id).into_string()
                    ),
                    Value::null(),
                )
            })?;
            let sig = b58(&r.signature);
            let id = if r.inner_index < 0 {
                format!("{sig}-{}", r.ix_index)
            } else {
                format!("{sig}-{}.{}", r.ix_index, r.inner_index)
            };
            nodes.push(ProgramInstruction {
                id: ID::new(id),
                program,
                tx_signature: sig,
                ix_index: r.ix_index as i32,
                inner_index: r.inner_index as i32,
                slot: I64(r.slot),
                block_time: r.block_time,
                ix_name: r.ix_name,
                accounts: r.accounts.iter().map(|a| b58(a)).collect(),
                data: r.data.to_string(),
            });
        }

        Ok(ProgramInstructionConnection {
            nodes,
            total_count: total_count_i32(total),
        })
    }

    // --- marketplace ------------------------------------------------------------------------

    /// The marketplace program's singleton config PDA.
    async fn marketplace_config(
        context: &GraphQLContext,
    ) -> FieldResult<Option<programs::marketplace::MarketplaceConfig>> {
        programs::marketplace::marketplace_config(context).await
    }

    /// Property listings on the marketplace. Each node's `propertyAsset` is LEFT-JOINed in
    /// (see `programs::marketplace::Listing`), so listings and their tokenised property come
    /// back in a single round-trip.
    async fn listings(
        context: &GraphQLContext,
        listing_id: Option<I64>,
        developer: Option<String>,
        status: Option<ListingStatus>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::ListingConnection> {
        programs::marketplace::listings(
            context, listing_id, developer, status, active, first, offset,
        )
        .await
    }

    /// Investors' per-listing positions.
    async fn investor_positions(
        context: &GraphQLContext,
        listing_id: Option<I64>,
        investor: Option<String>,
        cancelled: Option<bool>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::InvestorPositionConnection> {
        programs::marketplace::investor_positions(
            context, listing_id, investor, cancelled, active, first, offset,
        )
        .await
    }

    /// The marketplace's lawyer registry.
    async fn lawyers(
        context: &GraphQLContext,
        lawyer: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::LawyerConnection> {
        programs::marketplace::lawyers(context, lawyer, active, first, offset).await
    }

    /// SPV-lawyer election candidacies.
    async fn lawyer_candidacies(
        context: &GraphQLContext,
        listing_id: Option<I64>,
        lawyer: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::LawyerCandidacyConnection> {
        programs::marketplace::lawyer_candidacies(
            context, listing_id, lawyer, active, first, offset,
        )
        .await
    }

    /// SPV-lawyer election votes.
    async fn lawyer_votes(
        context: &GraphQLContext,
        listing_id: Option<I64>,
        voter: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::LawyerVoteConnection> {
        programs::marketplace::lawyer_votes(context, listing_id, voter, active, first, offset).await
    }

    /// Tokenised property assets backing listings. Each node nests the fetched-and-
    /// decomposed off-chain metadata document (`metadata`, ADR-27) when the enricher has
    /// one for the asset's PDA — asset + document in a single query (`metadata` is
    /// `null` while the fetch is pending or failing).
    async fn property_assets(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        region_id: Option<i32>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::PropertyAssetConnection> {
        programs::marketplace::property_assets(context, asset_id, region_id, active, first, offset)
            .await
    }

    /// Fetched and decomposed off-chain metadata for property assets (ADR-27): the indexer's
    /// background enricher downloads the JSON document each `metadataUri` points at; one row
    /// per asset, latest snapshot first.
    async fn property_metadata(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::PropertyMetadataConnection> {
        programs::marketplace::property_metadata(context, asset_id, first, offset).await
    }

    /// Per-token-account reservation totals.
    async fn reservations(
        context: &GraphQLContext,
        token_account: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::ReservationConnection> {
        programs::marketplace::reservations(context, token_account, active, first, offset).await
    }

    /// Investors' share holdings per asset.
    async fn share_holdings(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        owner: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::ShareHoldingConnection> {
        programs::marketplace::share_holdings(context, asset_id, owner, active, first, offset).await
    }

    /// Secondary-market share listings. `shareListingId` filters by the on-chain listing id
    /// (a separate id space from primary `listings`).
    async fn share_listings(
        context: &GraphQLContext,
        share_listing_id: Option<I64>,
        asset_id: Option<I64>,
        seller: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::ShareListingConnection> {
        programs::marketplace::share_listings(
            context,
            share_listing_id,
            asset_id,
            seller,
            active,
            first,
            offset,
        )
        .await
    }

    /// Bids on secondary-market share listings. `listingId` is the ShareListing id.
    async fn offers(
        context: &GraphQLContext,
        listing_id: Option<I64>,
        offeror: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::marketplace::OfferConnection> {
        programs::marketplace::offers(context, listing_id, offeror, active, first, offset).await
    }

    // --- property ---------------------------------------------------------------------------

    /// The property program's singleton config PDA.
    async fn property_config(
        context: &GraphQLContext,
    ) -> FieldResult<Option<programs::property::PropertyConfig>> {
        programs::property::property_config(context).await
    }

    /// Letting-agent election candidacies.
    async fn agent_candidacies(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        agent: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::AgentCandidacyConnection> {
        programs::property::agent_candidacies(context, asset_id, agent, active, first, offset).await
    }

    /// Letting-agent election votes.
    async fn agent_votes(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        voter: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::AgentVoteConnection> {
        programs::property::agent_votes(context, asset_id, voter, active, first, offset).await
    }

    /// Registered letting agents.
    async fn letting_agents(
        context: &GraphQLContext,
        wallet: Option<String>,
        region_id: Option<i32>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::LettingAgentConnection> {
        programs::property::letting_agents(context, wallet, region_id, active, first, offset).await
    }

    /// Per-property letting seats and elections.
    async fn property_lettings(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        agent: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::PropertyLettingConnection> {
        programs::property::property_lettings(context, asset_id, agent, active, first, offset).await
    }

    /// Letting agents' resignation notices.
    async fn resignation_notices(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        agent: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::ResignationNoticeConnection> {
        programs::property::resignation_notices(context, asset_id, agent, active, first, offset)
            .await
    }

    /// Holder spending proposals (above-low-tier only; auto-approved ones never reach
    /// storage).
    async fn proposals(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        proposer: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::ProposalConnection> {
        programs::property::proposals(context, asset_id, proposer, active, first, offset).await
    }

    /// Holder challenges against sitting letting agents.
    async fn challenges(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        challenger: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::ChallengeConnection> {
        programs::property::challenges(context, asset_id, challenger, active, first, offset).await
    }

    /// Votes on proposals and challenges (one account type behind both -- disambiguate by
    /// joining `voteId` against `proposals` / `challenges`).
    async fn gov_votes(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        voter: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::GovVoteConnection> {
        programs::property::gov_votes(context, asset_id, voter, active, first, offset).await
    }

    /// Per-property rental income ledgers.
    async fn property_incomes(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::PropertyIncomeConnection> {
        programs::property::property_incomes(context, asset_id, active, first, offset).await
    }

    /// Per-holder income claim checkpoints.
    async fn income_checkpoints(
        context: &GraphQLContext,
        asset_id: Option<I64>,
        owner: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::property::IncomeCheckpointConnection> {
        programs::property::income_checkpoints(context, asset_id, owner, active, first, offset)
            .await
    }

    // --- regions ----------------------------------------------------------------------------

    /// The regions program's singleton config PDA.
    async fn regions_config(
        context: &GraphQLContext,
    ) -> FieldResult<Option<programs::regions::RegionsConfig>> {
        programs::regions::regions_config(context).await
    }

    /// Governed regions.
    async fn regions(
        context: &GraphQLContext,
        region_id: Option<i32>,
        owner: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::regions::RegionConnection> {
        programs::regions::regions(context, region_id, owner, active, first, offset).await
    }

    /// Registered locations (postcodes) within regions.
    async fn locations(
        context: &GraphQLContext,
        region_id: Option<i32>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::regions::LocationConnection> {
        programs::regions::locations(context, region_id, active, first, offset).await
    }

    /// Region-creation proposals.
    async fn region_proposals(
        context: &GraphQLContext,
        region_id: Option<i32>,
        proposal_id: Option<I64>,
        proposer: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::regions::RegionProposalConnection> {
        programs::regions::region_proposals(
            context,
            region_id,
            proposal_id,
            proposer,
            active,
            first,
            offset,
        )
        .await
    }

    /// Per-region proposal-cycle state machines.
    async fn region_states(
        context: &GraphQLContext,
        region_id: Option<i32>,
        status: Option<RegionStatus>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::regions::RegionStateConnection> {
        programs::regions::region_states(context, region_id, status, active, first, offset).await
    }

    /// Votes on region proposals.
    async fn vote_records(
        context: &GraphQLContext,
        proposal_id: Option<I64>,
        voter: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::regions::VoteRecordConnection> {
        programs::regions::vote_records(context, proposal_id, voter, active, first, offset).await
    }

    // --- realxhub ---------------------------------------------------------------------------

    /// The realxhub program's singleton config PDA.
    async fn realxhub_config(
        context: &GraphQLContext,
    ) -> FieldResult<Option<programs::realxhub::RealxhubConfig>> {
        programs::realxhub::realxhub_config(context).await
    }

    /// Fractional hubs.
    async fn realxhub_hubs(
        context: &GraphQLContext,
        hub_id: Option<I64>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::realxhub::RealxhubHubConnection> {
        programs::realxhub::realxhub_hubs(context, hub_id, active, first, offset).await
    }

    /// Per-holder share holdings (the canonical ledger).
    async fn realxhub_holdings(
        context: &GraphQLContext,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::realxhub::RealxhubHoldingConnection> {
        programs::realxhub::realxhub_holdings(context, active, first, offset).await
    }

    /// Live secondary-market share listings.
    async fn realxhub_share_listings(
        context: &GraphQLContext,
        seller: Option<String>,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::realxhub::RealxhubShareListingConnection> {
        programs::realxhub::realxhub_share_listings(context, seller, active, first, offset).await
    }

    /// Per-wallet faucet cooldown receipts.
    async fn realxhub_faucet_receipts(
        context: &GraphQLContext,
        active: Option<bool>,
        first: Option<i32>,
        offset: Option<i32>,
    ) -> FieldResult<programs::realxhub::RealxhubFaucetReceiptConnection> {
        programs::realxhub::realxhub_faucet_receipts(context, active, first, offset).await
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
