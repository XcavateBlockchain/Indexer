//! The GraphQL API (spec Phase 5): the old SubQuery-shaped `schema.graphql` surface over Task 2's
//! parity views, GraphiQL, `/health`, and the mandatory DoS guards.
//!
//! Module map:
//!
//! | Module | Responsibility |
//! | --- | --- |
//! | [`config`] | Environment-driven process configuration. |
//! | [`db`] | The dedicated, statement-timeout-guarded read pool. |
//! | [`chain_tip`] | Cached `getSlot` reading, shared by `/health` and `syncStatus`. |
//! | [`graphql`] | The schema: `QueryRoot`, GraphQL object types, enums, context. |
//! | [`guards`] | The DoS guards: page-size clamps, query depth/complexity pre-parse. |
//! | [`health`] | `GET /health`. |
//! | [`router`] | Wires `/graphql`, `/graphiql`, `/health` into one Axum `Router`. |
//! | [`metrics`] | `GET /metrics` (Prometheus). |

mod chain_tip;
mod config;
mod db;
mod graphql;
mod guards;
mod health;
mod metrics;
mod router;
mod state;

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use crate::chain_tip::ChainTipCache;
use crate::config::Config;
use crate::graphql::{QueryRoot, Schema};
use crate::state::ApiState;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cfg = Config::from_env()?;
    log::debug!("{cfg:?}");

    let pool = db::connect(cfg.database_url()).await?;
    log::info!("connected the GraphQL read pool (statement_timeout=5s)");

    let chain_tip = Arc::new(ChainTipCache::new(cfg.rpc_endpoints()));

    // Reuses carbon-core's own GraphQL server plumbing (task-5-brief.md: "Use Carbon's graphql
    // module"). `build_schema` is fully generic over the query root / context, so it works with
    // this crate's own `QueryRoot` -- the generated decoder's QueryRoot (ruling R10, GENERATED
    // tables we do not populate) is never referenced.
    let schema: Arc<Schema> = carbon_core::graphql::server::build_schema(QueryRoot);

    let state = Arc::new(ApiState {
        pool,
        chain_tip,
        schema,
    });

    metrics::install(cfg.metrics_addr).context("starting the metrics listener")?;

    let app = router::build_router(state);

    let listener = TcpListener::bind(cfg.graphql_addr)
        .await
        .with_context(|| format!("binding {}", cfg.graphql_addr))?;
    log::info!(
        "GraphQL API listening on http://{} (POST /graphql, GET /graphiql, GET /health)",
        cfg.graphql_addr
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum server error")?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        let _ = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    log::info!("shutdown signal received");
}
