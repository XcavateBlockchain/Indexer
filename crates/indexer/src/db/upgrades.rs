//! Program version boundaries (`migrations/0011_program_upgrades.sql`, ADR-24).
//!
//! One row per (program, slot a bytecode version became live): the compiled-in deploy slot is
//! seeded at startup (source `'deploy'`), and every BPFLoaderUpgradeable `Upgrade` observed by
//! the recorder pipe (`crate::upgrades`) lands here through the batcher (source `'chain'`).
//! Append-only and idempotent -- backfill re-walks re-observe historical upgrades, and the
//! `ON CONFLICT DO NOTHING` makes that a no-op instead of a duplicate.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProgramUpgrade {
    pub upgrade_slot: i64,
    /// base58 transaction signature; `None` for the seeded deploy-slot row.
    pub signature: Option<String>,
    /// `'deploy'` (seeded from the registry) or `'chain'` (observed loader instruction).
    pub source: String,
    pub detected_at: DateTime<Utc>,
}

/// Seed one program's deploy-slot row. `ON CONFLICT DO NOTHING`: safe to call on every
/// process startup, like [`super::sync_state::init_sync_state`].
pub async fn seed_deploy_slot<'e, E>(
    executor: E,
    program_id: &[u8],
    deploy_slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO program_upgrades (program_id, upgrade_slot, signature, source)
        VALUES ($1, $2, NULL, 'deploy')
        ON CONFLICT (program_id, upgrade_slot) DO NOTHING
        "#,
        program_id,
        deploy_slot,
    )
    .execute(executor)
    .await
}

/// Record one observed on-chain upgrade. Returns `true` when the row is new -- the caller
/// uses that to bump the detection metric and log exactly once per boundary, no matter how
/// many crawl re-walks re-deliver the same upgrade transaction.
pub async fn record_upgrade<'e, E>(
    executor: E,
    program_id: &[u8],
    upgrade_slot: i64,
    signature: &str,
) -> Result<bool, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let result = sqlx::query!(
        r#"
        INSERT INTO program_upgrades (program_id, upgrade_slot, signature, source)
        VALUES ($1, $2, $3, 'chain')
        ON CONFLICT (program_id, upgrade_slot) DO NOTHING
        "#,
        program_id,
        upgrade_slot,
        signature,
    )
    .execute(executor)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// One program's full version timeline, oldest boundary first. The seeded deploy row makes
/// index N of this list the start of version N+1.
pub async fn upgrades_for<'e, E>(
    executor: E,
    program_id: &[u8],
) -> Result<Vec<ProgramUpgrade>, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query_as!(
        ProgramUpgrade,
        r#"
        SELECT upgrade_slot, signature, source, detected_at
        FROM program_upgrades
        WHERE program_id = $1
        ORDER BY upgrade_slot ASC
        "#,
        program_id,
    )
    .fetch_all(executor)
    .await
}
