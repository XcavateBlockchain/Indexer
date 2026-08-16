//! The GraphQL schema: the old SubQuery-shaped read surface over Task 2's views (task-5-brief.md).

pub mod context;
pub mod enums;
pub mod query;
pub mod types;

pub use context::GraphQLContext;
pub use query::QueryRoot;

/// `carbon_core::graphql::server::{DefaultMutation, DefaultSubscription}` are `EmptyMutation`/
/// `EmptySubscription` aliases -- this schema is query-only. Reusing carbon-core's aliases here
/// (rather than importing `juniper::{EmptyMutation, EmptySubscription}` directly) keeps this
/// schema's shape visibly tied to `carbon_core::graphql::server::build_schema`, which
/// constructs exactly a `RootNode<QueryRoot, DefaultMutation<C>, DefaultSubscription<C>>`.
pub type Schema = juniper::RootNode<
    'static,
    QueryRoot,
    carbon_core::graphql::server::DefaultMutation<GraphQLContext>,
    carbon_core::graphql::server::DefaultSubscription<GraphQLContext>,
>;
