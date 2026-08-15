//! Append-only writes to the `whitelist_actions` parity table (spec §5.4a, controller ruling
//! R7). Idempotent (`ON CONFLICT (id) DO NOTHING`) for the same reason `program_instructions`
//! is: reprocessing a transaction must be a no-op. Intended to be called by the instruction
//! processor (Task 3) in the same DB transaction as the corresponding `insert_instruction`
//! call -- both take a generic executor for exactly that reason (pass `&mut *tx`).

use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

use super::models::NewAction;

pub async fn insert_action<'e, E>(executor: E, row: NewAction) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let action_type = row.action_type.as_db_str();
    let role = row.role.map(|r| r.as_db_str());
    let permission = row.permission.map(|p| p.as_db_str());
    sqlx::query!(
        r#"
        INSERT INTO whitelist_actions
            (id, type, subject, role, permission, actor, slot, block_time, tx_signature, instruction_index)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (id) DO NOTHING
        "#,
        row.id,
        action_type,
        row.subject,
        role,
        permission,
        row.actor,
        row.slot,
        row.block_time,
        row.tx_signature,
        row.instruction_index,
    )
    .execute(executor)
    .await
}
