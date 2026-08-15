//! Sync-state singleton: the pipeline's own bookkeeping row, not on-chain data. See
//! `migrations/0003_sync_state.sql`.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SyncState {
    pub id: i16,
    pub last_contiguous_slot: i64,
    pub backfill_complete: bool,
    pub backfill_floor_slot: i64,
    pub snapshot_slot: Option<i64>,
    pub updated_at: DateTime<Utc>,
}

/// Seed the singleton row (`id = 1`). `ON CONFLICT DO NOTHING`: safe to call on every
/// process startup, only the very first call against an empty database actually does
/// anything.
pub async fn init_sync_state<'e, E>(
    executor: E,
    backfill_floor_slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO sync_state (id, last_contiguous_slot, backfill_complete, backfill_floor_slot, snapshot_slot)
        VALUES (1, $1, FALSE, $1, NULL)
        ON CONFLICT (id) DO NOTHING
        "#,
        backfill_floor_slot,
    )
    .execute(executor)
    .await
}

/// `last_contiguous_slot` is the highest slot below which there are no gaps, so -- like an
/// account write -- it must only move forward. Guarded the same way.
pub async fn advance_last_contiguous_slot<'e, E>(
    executor: E,
    slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        UPDATE sync_state
        SET last_contiguous_slot = $1, updated_at = now()
        WHERE id = 1 AND last_contiguous_slot < $1
        "#,
        slot,
    )
    .execute(executor)
    .await
}

/// Records the slot a `getProgramAccounts` snapshot was taken at. Driven by a single
/// backfill process (not subject to the multi-source races account writes are), so this is a
/// plain setter rather than a slot-guarded one.
pub async fn set_snapshot_slot<'e, E>(executor: E, slot: i64) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"UPDATE sync_state SET snapshot_slot = $1, updated_at = now() WHERE id = 1"#,
        slot,
    )
    .execute(executor)
    .await
}

pub async fn set_backfill_complete<'e, E>(
    executor: E,
    complete: bool,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"UPDATE sync_state SET backfill_complete = $1, updated_at = now() WHERE id = 1"#,
        complete,
    )
    .execute(executor)
    .await
}

pub async fn get_sync_state<'e, E>(executor: E) -> Result<Option<SyncState>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query_as!(
        SyncState,
        r#"SELECT id, last_contiguous_slot, backfill_complete, backfill_floor_slot, snapshot_slot, updated_at FROM sync_state WHERE id = 1"#
    )
    .fetch_optional(executor)
    .await
}
