//! Full in-place reset: wipe every indexed row so the next `indexer run` re-indexes from
//! scratch -- the volume-drop procedure of RUNBOOK.md "Devnet ledger reset" without touching
//! the `pgdata` volume (which also holds the inert SubQuery rollback schema, ADR-21).
//!
//! One multi-table `TRUNCATE` covers:
//!
//! * every account-state table ([`StateTable::ALL`]) -- the next startup snapshot rewrites
//!   them through the slot-guarded upserts;
//! * the append-only history tables (`program_instructions`, `whitelist_actions`) -- the next
//!   startup backfill re-walks them from each program's deploy slot;
//! * the webhook outbox (`webhook_events`) -- wiping it is what makes the reindex RE-DELIVER
//!   every `init_property_assets` registration (the batcher re-records them during the
//!   backfill, the delivery loop flushes the backlog; RUNBOOK.md expects that burst);
//! * the sold-out notification outbox (`sold_out_notifications`) -- same re-delivery
//!   argument as the webhook outbox: the reindex re-derives every SOLD_OUT listing upsert,
//!   the batcher re-records the events, and the loop re-announces any reserver who still
//!   has not claimed (the send-time reserver query skips those who have);
//! * the derived tables (`marketplace_property_metadata`, `marketplace_property_image`) --
//!   the metadata fetcher and image mirror refill them on their own schedules;
//! * the bookkeeping tables (`sync_state`, `backfill_cursor`, `program_upgrades`) --
//!   `main.rs`'s `start()` re-seeds `sync_state` (`snapshot_slot = NULL`,
//!   `backfill_complete = FALSE`, floor = deploy slot) and `program_upgrades` on the next
//!   start of any subcommand, which is exactly what makes the automatic startup
//!   snapshot+backfill fire.
//!
//! Deliberately NOT touched: `_sqlx_migrations` (the schema itself stays migrated) and the
//! SubQuery rollback schema `app` in the same database. No foreign keys exist between these
//! tables (grep `migrations/` for REFERENCES), so a plain TRUNCATE needs no CASCADE.
//!
//! The indexer must be STOPPED while this runs: a live process would re-seed `sync_state`
//! mid-truncate and could slot-guard-upsert rows back into freshly emptied tables.

use sqlx::postgres::{PgPool, PgQueryResult};

use super::close::StateTable;

/// Tables outside the [`StateTable`] roster that a full reset also wipes.
const EXTRA_TABLES: &[&str] = &[
    "program_instructions",
    "whitelist_actions",
    "webhook_events",
    "sold_out_notifications",
    "marketplace_property_metadata",
    "marketplace_property_image",
    "program_upgrades",
    "sync_state",
    "backfill_cursor",
];

/// The complete wipe list, account-state tables first.
pub fn all_tables() -> Vec<&'static str> {
    StateTable::ALL
        .iter()
        .map(|t| t.table_name())
        .chain(EXTRA_TABLES.iter().copied())
        .collect()
}

/// Truncate every table in [`all_tables`]. Returns the number of tables truncated. Table
/// names come only from the compile-time constants above, so nothing user-controlled ever
/// reaches the SQL string -- the same argument `close_in_table` makes for its dynamic SQL.
pub async fn reset_all(pool: &PgPool) -> Result<usize, sqlx::Error> {
    let tables = all_tables();
    let sql = format!("TRUNCATE TABLE {}", tables.join(", "));
    let _: PgQueryResult = sqlx::query(&sql).execute(pool).await?;
    Ok(tables.len())
}
