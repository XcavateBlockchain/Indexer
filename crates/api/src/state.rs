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
}
