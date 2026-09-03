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
//! | [`graphiql`] | `GET /graphiql`: the IDE page (juniper 0.16.2's stock page is broken). |
//! | [`graphql`] | The schema: `QueryRoot`, GraphQL object types, enums, context. |
//! | [`guards`] | The DoS guards: page-size clamps, query depth/complexity pre-parse. |
//! | [`health`] | `GET /health`. |
//! | [`router`] | Wires `/graphql`, `/graphiql`, `/health` into one Axum `Router`. |
//! | [`metrics`] | `GET /metrics` (Prometheus). |

mod chain_tip;
mod config;
mod db;
mod graphiql;
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
    // ADR-32: install the process-level rustls CryptoProvider before any TLS use — the shared
    // dependency graph enables both provider crate-features, and rustls 0.23 panics at first
    // TLS use without an explicit install (see crates/indexer/src/main.rs for the full
    // rationale). This process's first TLS use is the chain-tip getSlot
    // (solana-rpc-client -> reqwest -> rustls); without the install, every cache-expiry RPC
    // call would panic inside the request task and 500 /health and syncStatus.
    let _ = rustls::crypto::ring::default_provider().install_default();

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

    let program_filter = cfg.programs.as_ref().map(|programs| {
        log::info!(
            "PROGRAMS scope: sync aggregates cover {}",
            programs
                .iter()
                .map(|p| p.registry_name())
                .collect::<Vec<_>>()
                .join(",")
        );
        programs.iter().map(|p| p.as_program_id_bytes()).collect()
    });
    let state = Arc::new(ApiState {
        pool,
        chain_tip,
        schema,
        program_filter,
    });

    metrics::install(cfg.metrics_addr).context("starting the metrics listener")?;

    match &cfg.cors_allowed_origins {
        None => log::info!("CORS: allowing every origin (set CORS_ALLOWED_ORIGINS to restrict)"),
        Some(origins) => log::info!("CORS: restricted to {origins:?}"),
    }
    let app = router::build_router(state, cfg.cors_allowed_origins.clone());

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
