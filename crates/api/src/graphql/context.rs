use std::sync::Arc;

use sqlx::PgPool;

use crate::chain_tip::ChainTipCache;

#[derive(Clone)]
pub struct GraphQLContext {
    pub pool: PgPool,
    pub chain_tip: Arc<ChainTipCache>,
    /// The `PROGRAMS` scope as raw 32-byte program ids (`None` = no scoping); used by
    /// `syncStatus` to keep its aggregates honest under subset operation.
    pub program_filter: Option<Vec<Vec<u8>>>,
}

impl juniper::Context for GraphQLContext {}
