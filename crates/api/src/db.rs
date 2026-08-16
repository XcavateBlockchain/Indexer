//! The GraphQL API's own connection pool -- deliberately NOT `indexer::db::connect` (that
//! module belongs to a different crate entirely, and even if it were reusable, the brief is
//! explicit: this must be a DEDICATED pool with resolver-appropriate settings, not the write
//! pipeline's). Two differences from a plausible write-pool config:
//!
//! 1. `after_connect` sets `statement_timeout = '5s'` on every connection -- a runaway or
//!    accidentally-unindexed resolver query gets killed by Postgres itself rather than tying up
//!    a pool connection (and this crate's tokio task) indefinitely. The indexer's write pool
//!    carries no such timeout (a slow migration or a legitimately large backfill commit must not
//!    be killed).
//! 2. Smaller `max_connections` -- this is a read-only, request-scoped fan-out, not a single
//!    background writer.

use anyhow::{Context, Result};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Executor as _, PgPool};

/// Read-pool size. Generous enough for a handful of concurrent GraphQL requests (each resolver
/// issues at most two queries -- a page and a count) without needing to be tuned per deployment;
/// small enough that a stuck query (caught by the 5s statement timeout regardless) can't starve
/// the whole pool for long.
const MAX_CONNECTIONS: u32 = 10;

pub async fn connect(database_url: &str) -> Result<PgPool> {
    let options: PgConnectOptions = database_url
        .parse()
        .with_context(|| "DATABASE_URL is not a valid postgres:// connection string")?;

    PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                conn.execute("SET statement_timeout = '5s'").await?;
                Ok(())
            })
        })
        .connect_with(options)
        .await
        .context("connecting the GraphQL read pool")
}
