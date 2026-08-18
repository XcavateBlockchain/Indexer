//! Sync-state rows: the pipeline's own bookkeeping, not on-chain data. One row per indexed
//! program (keyed by `program_id`) since `migrations/0007_multi_program_sync.sql`; see
//! `migrations/0003_sync_state.sql` for the row's meaning.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SyncState {
    pub program_id: Vec<u8>,
    pub last_contiguous_slot: i64,
    pub backfill_complete: bool,
    pub backfill_floor_slot: i64,
    pub snapshot_slot: Option<i64>,
    pub updated_at: DateTime<Utc>,
}

/// Seed one program's row. `ON CONFLICT DO NOTHING`: safe to call on every process startup,
/// only the first call for a given program against this database actually does anything.
pub async fn init_sync_state<'e, E>(
    executor: E,
    program_id: &[u8],
    backfill_floor_slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO sync_state (program_id, last_contiguous_slot, backfill_complete, backfill_floor_slot, snapshot_slot)
        VALUES ($1, $2, FALSE, $2, NULL)
        ON CONFLICT (program_id) DO NOTHING
        "#,
        program_id,
        backfill_floor_slot,
    )
    .execute(executor)
    .await
}

/// `last_contiguous_slot` is the highest slot below which there are no gaps in this
/// program's history, so -- like an account write -- it must only move forward. Guarded the
/// same way.
pub async fn advance_last_contiguous_slot<'e, E>(
    executor: E,
    program_id: &[u8],
    slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        UPDATE sync_state
        SET last_contiguous_slot = $2, updated_at = now()
        WHERE program_id = $1 AND last_contiguous_slot < $2
        "#,
        program_id,
        slot,
    )
    .execute(executor)
    .await
}

/// Records the slot a program's `getProgramAccounts` snapshot was taken at. Driven by a
/// single backfill process (not subject to the multi-source races account writes are), so
/// this is a plain setter rather than a slot-guarded one.
pub async fn set_snapshot_slot<'e, E>(
    executor: E,
    program_id: &[u8],
    slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"UPDATE sync_state SET snapshot_slot = $2, updated_at = now() WHERE program_id = $1"#,
        program_id,
        slot,
    )
    .execute(executor)
    .await
}

pub async fn set_backfill_complete<'e, E>(
    executor: E,
    program_id: &[u8],
    complete: bool,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"UPDATE sync_state SET backfill_complete = $2, updated_at = now() WHERE program_id = $1"#,
        program_id,
        complete,
    )
    .execute(executor)
    .await
}

pub async fn get_sync_state<'e, E>(
    executor: E,
    program_id: &[u8],
) -> Result<Option<SyncState>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query_as!(
        SyncState,
        r#"
        SELECT program_id, last_contiguous_slot, backfill_complete, backfill_floor_slot, snapshot_slot, updated_at
        FROM sync_state WHERE program_id = $1
        "#,
        program_id,
    )
    .fetch_optional(executor)
    .await
}
