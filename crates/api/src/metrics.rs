//! Prometheus metrics for the GraphQL API: `GET /metrics` on `METRICS_ADDR` (default
//! `0.0.0.0:9465` -- deliberately different from the indexer binary's `9464` default, brief
//! requirement, so the two processes' exporters never collide on one host).
//!
//! A minimal local copy of `crates/indexer/src/metrics.rs`'s exporter-install shape, not a
//! shared module (brief: "reuse Task 3's exporter module if it is reusable across binaries, else
//! a minimal local copy"). It is not a straight reuse because the two binaries' metric sets are
//! disjoint: the indexer module wires up `carbon_core::metrics::Metrics` (irrelevant here -- this
//! crate never runs a carbon pipeline) and registers indexer-only series
//! (`decode_skipped_total`, `db_flush_*`, ...). Extracting a shared "just the `install(addr)`
//! plumbing" module would save ~15 lines of `PrometheusBuilder::new().with_http_listener(...)`
//! at the cost of a cross-crate dependency edge between two otherwise-independent binaries;
//! given the metric *names* still have to be declared per-binary either way, the duplication is
//! smaller than the coupling it would buy.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use metrics_exporter_prometheus::PrometheusBuilder;

/// Counter: total GraphQL requests received on `/graphql`.
pub const GRAPHQL_REQUESTS_TOTAL: &str = "graphql_requests_total";
/// Histogram: wall time of one `/graphql` request (parse-guard + juniper execution).
pub const GRAPHQL_REQUEST_DURATION_SECONDS: &str = "graphql_request_duration_seconds";
/// Counter: requests rejected before execution, labelled `reason=depth|complexity|parse`.
pub const GRAPHQL_REJECTED_TOTAL: &str = "graphql_rejected_total";

/// Installs the global recorder and starts the `GET /metrics` listener on `addr`. Must be
/// called from inside a Tokio runtime (the listener is a spawned task); calling it twice in one
/// process is an error (the global recorder can only be set once).
pub fn install(addr: SocketAddr) -> Result<()> {
    PrometheusBuilder::new()
        .with_http_listener(addr)
        .set_buckets_for_metric(
            metrics_exporter_prometheus::Matcher::Full(
                GRAPHQL_REQUEST_DURATION_SECONDS.to_string(),
            ),
            &[
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ],
        )
        .context("invalid bucket list for graphql_request_duration_seconds")?
        .install()
        .with_context(|| format!("failed to start the metrics listener on {addr}"))?;

    metrics::describe_counter!(
        GRAPHQL_REQUESTS_TOTAL,
        "Total GraphQL requests received on /graphql"
    );
    metrics::describe_histogram!(
        GRAPHQL_REQUEST_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Wall time of one /graphql request"
    );
    metrics::describe_counter!(
        GRAPHQL_REJECTED_TOTAL,
        "Requests rejected by the depth/complexity/parse guard before execution"
    );

    // Register every counter (and label value) at zero so a Prometheus rule like
    // `rate(graphql_rejected_total[5m]) > 0` reads "healthy" rather than "no data" before the
    // first occurrence (same reasoning as crates/indexer/src/metrics.rs).
    metrics::counter!(GRAPHQL_REQUESTS_TOTAL).increment(0);
    for reason in ["depth", "complexity", "parse"] {
        metrics::counter!(GRAPHQL_REJECTED_TOTAL, "reason" => reason).increment(0);
    }

    log::info!("metrics listener started on http://{addr}/metrics");
    Ok(())
}

pub fn inc_requests() {
    metrics::counter!(GRAPHQL_REQUESTS_TOTAL).increment(1);
}

pub fn observe_duration(d: Duration) {
    metrics::histogram!(GRAPHQL_REQUEST_DURATION_SECONDS).record(d.as_secs_f64());
}

/// `reason` is one of `depth`, `complexity`, `parse` (see [`crate::guards::RejectReason`]).
pub fn inc_rejected(reason: &'static str) {
    metrics::counter!(GRAPHQL_REJECTED_TOTAL, "reason" => reason).increment(1);
}
