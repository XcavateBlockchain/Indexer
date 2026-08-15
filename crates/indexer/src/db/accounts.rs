//! Slot-guarded upserts and soft closes for the three account-state tables. Every upsert
//! follows the pattern mandated by spec §5.2:
//!
//! ```sql
//! INSERT INTO <table> (...) VALUES (...)
//! ON CONFLICT (pubkey) DO UPDATE SET ... WHERE <table>.slot < EXCLUDED.slot
//! ```
//!
//! Without the `WHERE` guard, a `getProgramAccounts` snapshot load racing a live-stream
//! reconnect can silently overwrite fresh state with stale state -- no error, no constraint
//! violation, just a row that quietly goes backwards. This is the single most important
//! property this module has to hold; see `db::tests` for the mandated slot-guard test.
//!
//! Closes are soft (`closed_at_slot`), guarded the same way, and never `DELETE`. Note that
//! `close_*` can only guard against *older* writes -- it has no data to synthesize a full row
//! from if the account was never seen created (Carbon's `AccountDeletion` carries only
//! `{pubkey, slot, transaction_signature}`), so a close on an unknown pubkey is a silent
//! no-op. That's a limitation of the deletion event shape, not of the guard itself.

use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

use super::models::{AdminAccount, ConfigAccount, RoleAccountRow};

pub async fn upsert_config<'e, E>(executor: E, row: ConfigAccount) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO config (pubkey, slot, lamports, closed_at_slot, authority, pending_authority, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot              = EXCLUDED.slot,
            lamports          = EXCLUDED.lamports,
            closed_at_slot    = EXCLUDED.closed_at_slot,
            authority         = EXCLUDED.authority,
            pending_authority = EXCLUDED.pending_authority,
            bump              = EXCLUDED.bump
        WHERE config.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.authority,
        row.pending_authority,
        row.bump,
    )
    .execute(executor)
    .await
}

/// Slot-guarded soft close: sets `closed_at_slot` (and bumps `slot`, so a later stale write
/// can't undo the close) only if `slot` currently stored is older than `slot` passed in.
pub async fn close_config<'e, E>(executor: E, pubkey: &[u8], slot: i64) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"UPDATE config SET slot = $2, closed_at_slot = $2 WHERE pubkey = $1 AND slot < $2"#,
        pubkey,
        slot,
    )
    .execute(executor)
    .await
}

pub async fn upsert_admin<'e, E>(executor: E, row: AdminAccount) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO admin (pubkey, slot, lamports, closed_at_slot, admin, bump)
        VALUES ($1, $2, $3, NULL, $4, $5)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            admin          = EXCLUDED.admin,
            bump           = EXCLUDED.bump
        WHERE admin.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.admin,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn close_admin<'e, E>(executor: E, pubkey: &[u8], slot: i64) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"UPDATE admin SET slot = $2, closed_at_slot = $2 WHERE pubkey = $1 AND slot < $2"#,
        pubkey,
        slot,
    )
    .execute(executor)
    .await
}

pub async fn upsert_role_account<'e, E>(executor: E, row: RoleAccountRow) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let role = row.role.as_db_str();
    let permission = row.permission.as_db_str();
    sqlx::query!(
        r#"
        INSERT INTO role_account (pubkey, slot, lamports, closed_at_slot, user_pubkey, role, permission, rent_payer, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            user_pubkey    = EXCLUDED.user_pubkey,
            role           = EXCLUDED.role,
            permission     = EXCLUDED.permission,
            rent_payer     = EXCLUDED.rent_payer,
            bump           = EXCLUDED.bump
        WHERE role_account.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.user_pubkey,
        role,
        permission,
        row.rent_payer,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn close_role_account<'e, E>(executor: E, pubkey: &[u8], slot: i64) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"UPDATE role_account SET slot = $2, closed_at_slot = $2 WHERE pubkey = $1 AND slot < $2"#,
        pubkey,
        slot,
    )
    .execute(executor)
    .await
}
