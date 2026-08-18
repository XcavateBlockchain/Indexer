use std::sync::Arc;

use sqlx::PgPool;

use crate::chain_tip::ChainTipCache;
use crate::graphql::Schema;

/// Shared Axum application state: the dedicated read pool, the cached chain-tip reader (shared
/// between `/health` and `syncStatus`), and the built GraphQL schema.
pub struct ApiState {
    pub pool: PgPool,
    pub chain_tip: Arc<ChainTipCache>,
    pub schema: Arc<Schema>,
    /// The `PROGRAMS` scope as raw 32-byte program ids, ready to bind as `bytea[]`:
    /// `None` = no scoping (report every sync row). See `Config::programs`.
    pub program_filter: Option<Vec<Vec<u8>>>,
}
