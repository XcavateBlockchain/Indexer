//! Builds the Axum app: `POST/GET /graphql`, `GET /graphiql`, `GET /health`.
//!
//! `POST /graphql` deliberately does NOT use `carbon_core::graphql::server::graphql_router` --
//! that helper's handler extracts a [`JuniperRequest`] and calls `req.execute(...)` in one step,
//! with no seam to run the depth/complexity pre-parse guard before execution. This module reuses
//! the same building blocks `graphql_router` is built from (`juniper_axum`'s
//! [`JuniperRequest`]/[`JuniperResponse`]) directly, plus
//! [`crate::guards::check_query`] run against the raw query text before anything reaches juniper.
//! `carbon_core::graphql::server::build_schema` IS reused as-is (see `main.rs`) -- it has no such
//! conflict, since building a schema has nothing to do with per-request guarding.

use std::sync::Arc;
use std::time::Instant;

use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, on, MethodFilter};
use axum::Router;
use tower_http::cors::{AllowOrigin, Any, CorsLayer};
use juniper::http::{GraphQLBatchRequest, GraphQLBatchResponse, GraphQLResponse};
use juniper::{FieldError, Value};
use juniper_axum::extract::JuniperRequest;
use juniper_axum::response::JuniperResponse;

use crate::graphql::GraphQLContext;
use crate::guards::{self, Rejection};
use crate::state::ApiState;
use crate::{health, metrics};

pub fn build_router(
    state: Arc<ApiState>,
    cors_allowed_origins: Option<Vec<HeaderValue>>,
) -> Router {
    Router::new()
        .route(
            "/graphql",
            on(MethodFilter::GET.or(MethodFilter::POST), graphql_handler),
        )
        .route("/graphiql", get(crate::graphiql::graphiql))
        .route("/health", get(health::health))
        .layer(cors_layer(cors_allowed_origins))
        .with_state(state)
}

/// Browser cross-origin access: every origin by default, or only the `CORS_ALLOWED_ORIGINS`
/// list when configured (see [`crate::config`]). Methods and headers are always unrestricted
/// -- the API is read-only and unauthenticated (no cookies/credentials ever), so the origin
/// list is the only knob worth having. The layer also answers `OPTIONS` preflights itself,
/// which is why no `OPTIONS` route appears above.
fn cors_layer(allowed_origins: Option<Vec<HeaderValue>>) -> CorsLayer {
    let origin = match allowed_origins {
        None => AllowOrigin::any(),
        Some(list) => AllowOrigin::list(list),
    };
    CorsLayer::new()
        .allow_origin(origin)
        .allow_methods(Any)
        .allow_headers(Any)
}

async fn graphql_handler(
    axum::extract::State(state): axum::extract::State<Arc<ApiState>>,
    JuniperRequest(request): JuniperRequest,
) -> Response {
    let start = Instant::now();
    metrics::inc_requests();

    if let Some(rejection) = first_rejection(&request) {
        metrics::inc_rejected(rejection.reason.as_label());
        metrics::observe_duration(start.elapsed());
        log::warn!(
            "graphql: rejected before execution (reason={}): {}",
            rejection.reason.as_label(),
            rejection.message
        );
        return rejection_response(&request, rejection).into_response();
    }

    let context = GraphQLContext {
        pool: state.pool.clone(),
        chain_tip: state.chain_tip.clone(),
    };
    let response = request.execute(&state.schema, &context).await;
    metrics::observe_duration(start.elapsed());
    JuniperResponse(response).into_response()
}

/// Checks every operation in the request (a batch is rejected if ANY of its operations would
/// be) and returns the first violation found.
fn first_rejection(request: &GraphQLBatchRequest) -> Option<Rejection> {
    match request {
        GraphQLBatchRequest::Single(single) => guards::check_query(&single.query).err(),
        GraphQLBatchRequest::Batch(batch) => batch
            .iter()
            .find_map(|single| guards::check_query(&single.query).err()),
    }
}

/// A GraphQL-shaped error response, mirroring the request's single/batch shape, built the same
/// way juniper's own `GraphQLResponse::error` constructs an out-of-band error (`data: null`,
/// `errors: [...]`) -- so a guard rejection looks exactly like any other GraphQL error to a
/// client, just with a message identifying it as a pre-execution rejection.
fn rejection_response(request: &GraphQLBatchRequest, rejection: Rejection) -> JuniperResponse {
    let error = FieldError::new(rejection.message, Value::null());
    let response = match request {
        GraphQLBatchRequest::Batch(batch) => GraphQLBatchResponse::Batch(
            batch
                .iter()
                .map(|_| GraphQLResponse::error(error.clone()))
                .collect(),
        ),
        GraphQLBatchRequest::Single(_) => {
            GraphQLBatchResponse::Single(GraphQLResponse::error(error))
        }
    };
    JuniperResponse(response)
}
