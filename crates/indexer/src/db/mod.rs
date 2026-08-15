//! Storage layer: pool construction + migration runner, plus the slot-guarded upserts,
//! append-only inserts, and sync-state helpers that Task 3's pipeline processors (and,
//! read-only, Task 5's GraphQL API) build on.
//!
//! The schema itself lives in `migrations/` at the repo root (checked-in `sqlx` migrations,
//! `sqlx::migrate!()`-compatible). This module is the *only* place in the crate that is
//! allowed to write to it -- every write goes through one of the functions here, so the
//! slot-guard and idempotency invariants only have to be gotten right once.
//!
//! Controller ruling R10: the generated decoder crate (`carbon-xcavate-whitelist-decoder`)
//! ships its own postgres artifacts (sqlx_migrator migrations, un-slot-guarded upserts against
//! `*_account`-style tables with `__pubkey` columns). None of that is used here. This module
//! and the `migrations/` directory are the only schema.

pub mod accounts;
pub mod actions;
pub mod instructions;
pub mod models;
pub mod sync_state;

#[cfg(test)]
mod tests;

use sqlx::postgres::{PgPool, PgPoolOptions};

/// Build a connection pool from a `postgres://` URL (typically `DATABASE_URL`).
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(10)
        .connect(database_url)
        .await
}

/// Apply every checked-in migration under `migrations/` (repo root) that hasn't run yet.
/// Idempotent: `sqlx::migrate!` tracks applied versions in `_sqlx_migrations`, so calling
/// this on every process startup is the intended usage, not just a one-time setup step.
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("../../migrations").run(pool).await
}
