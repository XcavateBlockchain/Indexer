use std::sync::Arc;

use sqlx::PgPool;

use crate::chain_tip::ChainTipCache;

#[derive(Clone)]
pub struct GraphQLContext {
    pub pool: PgPool,
    pub chain_tip: Arc<ChainTipCache>,
}

impl juniper::Context for GraphQLContext {}
