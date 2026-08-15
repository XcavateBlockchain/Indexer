//! The history backfill's resume cursor (`migrations/0006_backfill_cursor.sql`).
//!
//! Singleton row, written by the backfill after every fully-committed page of signatures and
//! deleted when a walk reaches its stop condition. See [`crate::backfill`] for the semantics;
//! this module is only the SQL.

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

/// Upsert the cursor. Unconditional (no slot guard): the backfill is a single walker moving
/// strictly downwards, and a re-run that starts from the tip legitimately moves the cursor back
/// up before walking down past it again.
pub async fn set_cursor<'e, E>(
    executor: E,
    signature: &str,
    slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO backfill_cursor (id, signature, slot, updated_at)
        VALUES (1, $1, $2, now())
        ON CONFLICT (id) DO UPDATE SET signature = EXCLUDED.signature,
                                       slot = EXCLUDED.slot,
                                       updated_at = now()
        "#,
        signature,
        slot,
    )
    .execute(executor)
    .await
}

pub async fn get_cursor<'e, E>(executor: E) -> Result<Option<BackfillCursor>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query_as!(
        BackfillCursor,
        r#"SELECT signature, slot, updated_at FROM backfill_cursor WHERE id = 1"#
    )
    .fetch_optional(executor)
    .await
}

/// Drop the cursor: the walk finished, so there is nothing to resume.
pub async fn clear_cursor<'e, E>(executor: E) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(r#"DELETE FROM backfill_cursor WHERE id = 1"#)
        .execute(executor)
        .await
}
