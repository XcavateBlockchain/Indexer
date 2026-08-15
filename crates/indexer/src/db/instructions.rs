//! Append-only instruction-history writes. `ON CONFLICT DO NOTHING` on the composite primary
//! key is what makes replaying the same transaction (stream reconnect, backfill/live overlap)
//! a no-op -- see `migrations/0001_program_instructions.sql`.

use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

use super::models::NewInstruction;

pub async fn insert_instruction<'e, E>(executor: E, row: NewInstruction) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO program_instructions
            (signature, ix_index, inner_index, slot, block_time, ix_name, accounts, data)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (signature, ix_index, inner_index) DO NOTHING
        "#,
        row.signature,
        row.ix_index,
        row.inner_index,
        row.slot,
        row.block_time,
        row.ix_name,
        &row.accounts,
        row.data,
    )
    .execute(executor)
    .await
}
