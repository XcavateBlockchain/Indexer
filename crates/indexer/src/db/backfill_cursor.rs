//! The history backfill's resume cursors (`migrations/0006_backfill_cursor.sql`, re-keyed
//! per program by `migrations/0007_multi_program_sync.sql`).
//!
//! One row per program with an interrupted walk, written by that program's backfill after
//! every fully-committed page of signatures and deleted when the walk reaches its stop
//! condition. See [`crate::backfill`] for the semantics; this module is only the SQL.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BackfillCursor {
    /// base58 signature to pass as `before` on the next `getSignaturesForAddress` call.
    pub signature: String,
    pub slot: i64,
    pub updated_at: DateTime<Utc>,
}

/// Upsert one program's cursor. Unconditional (no slot guard): a backfill is a single walker
/// moving strictly downwards through one program's history, and a re-run that starts from the
/// tip legitimately moves the cursor back up before walking down past it again.
pub async fn set_cursor<'e, E>(
    executor: E,
    program_id: &[u8],
    signature: &str,
    slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO backfill_cursor (program_id, signature, slot, updated_at)
        VALUES ($1, $2, $3, now())
        ON CONFLICT (program_id) DO UPDATE SET signature = EXCLUDED.signature,
                                               slot = EXCLUDED.slot,
                                               updated_at = now()
        "#,
        program_id,
        signature,
        slot,
    )
    .execute(executor)
    .await
}

pub async fn get_cursor<'e, E>(
    executor: E,
    program_id: &[u8],
) -> Result<Option<BackfillCursor>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query_as!(
        BackfillCursor,
        r#"SELECT signature, slot, updated_at FROM backfill_cursor WHERE program_id = $1"#,
        program_id,
    )
    .fetch_optional(executor)
    .await
}

/// Drop one program's cursor: its walk finished, so there is nothing to resume.
pub async fn clear_cursor<'e, E>(
    executor: E,
    program_id: &[u8],
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"DELETE FROM backfill_cursor WHERE program_id = $1"#,
        program_id,
    )
    .execute(executor)
    .await
}
