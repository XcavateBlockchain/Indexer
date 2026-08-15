//! Prometheus metrics: a `carbon_core::metrics::Metrics` implementation on top of the
//! `metrics` facade, plus a `GET /metrics` listener from `metrics-exporter-prometheus`.
//!
//! Why not `carbon-prometheus-metrics`? It hard-binds `127.0.0.1:9100`, which is unreachable
//! from a sibling container in the compose stack and not overridable (ruling R12). This module
//! is ~100 lines and lets the listen address come from `METRICS_ADDR`.
//!
//! Everything carbon-core's pipeline records (`updates_received`, `updates_successful`,
//! `updates_failed`, `updates_processed`, `updates_queued`, `updates_process_time_*`,
//! `account_updates_processed`, `transaction_updates_processed`,
//! `account_deletions_processed`, plus the datasources' own counters) arrives through
//! [`PrometheusMetrics`] and is exported unprefixed, exactly as carbon names it. The
//! indexer's own metrics are the `pub fn`s at the bottom of this file.
//!
//! If [`install`] is never called (unit tests, `replay` runs without a metrics port) the
//! `metrics` macros fall through to the global no-op recorder, so every call site here stays
//! valid and costs nothing.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use carbon_core::error::CarbonResult;
use carbon_core::metrics::Metrics;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

/// Counter: instruction updates that decoded but could not be mapped to a row.
pub const DECODE_SKIPPED_TOTAL: &str = "decode_skipped_total";
/// Counter: gRPC stream sessions that ended and had to be rebuilt.
pub const GRPC_RECONNECTS_TOTAL: &str = "grpc_reconnects_total";
/// Counter: block-time resolutions, labelled by where the answer came from.
pub const BLOCK_TIME_LOOKUPS_TOTAL: &str = "block_time_lookups_total";
/// Histogram: wall time of one batch flush transaction.
pub const DB_FLUSH_DURATION_SECONDS: &str = "db_flush_duration_seconds";
/// Histogram: number of write ops in one batch flush.
pub const DB_FLUSH_ROWS: &str = "db_flush_rows";

/// Installs the global recorder and starts the `GET /metrics` listener on `addr`.
///
/// Must be called from inside a Tokio runtime (the listener is a spawned task). Calling it
/// twice in one process is an error -- the global recorder can only be set once.
pub fn install(addr: SocketAddr) -> Result<()> {
    PrometheusBuilder::new()
        .with_http_listener(addr)
        // Explicit buckets for the two histograms whose ranges we actually know. Everything
        // else (carbon's nanosecond timings, the datasources' millisecond timings) keeps the
        // exporter's default summary/quantile treatment, which needs no range guess.
        .set_buckets_for_metric(
            Matcher::Full(DB_FLUSH_DURATION_SECONDS.to_string()),
            &[
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ],
        )
        .context("invalid bucket list for db_flush_duration_seconds")?
        .set_buckets_for_metric(
            Matcher::Full(DB_FLUSH_ROWS.to_string()),
            &[1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0],
        )
        .context("invalid bucket list for db_flush_rows")?
        .install()
        .with_context(|| format!("failed to start the metrics listener on {addr}"))?;

    metrics::describe_counter!(
        DECODE_SKIPPED_TOTAL,
        "Instruction updates that decoded but could not be mapped to a whitelist_actions row"
    );
    metrics::describe_counter!(
        GRPC_RECONNECTS_TOTAL,
        "Yellowstone gRPC stream sessions that ended and were rebuilt"
    );
    metrics::describe_counter!(
        BLOCK_TIME_LOOKUPS_TOTAL,
        "Block-time resolutions by source (stream hint, in-process cache, primary RPC, fallback RPC)"
    );
    metrics::describe_histogram!(
        DB_FLUSH_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Wall time of one batch flush transaction"
    );
    metrics::describe_histogram!(
        DB_FLUSH_ROWS,
        "Write operations committed in one batch flush"
    );

    // Register every counter (and every label value it can take) at zero. Without this the
    // series simply does not exist until the first occurrence, and a Prometheus rule like
    // `rate(decode_skipped_total[5m]) > 0` reads "no data" rather than "healthy" -- which is
    // exactly backwards for metrics whose whole purpose is to be zero.
    inc_grpc_reconnect_by(0);
    for reason in ["missing_account", "empty_absolute_path", "serialize"] {
        metrics::counter!(DECODE_SKIPPED_TOTAL, "reason" => reason).increment(0);
    }
    for source in ["stream", "cache", "rpc", "rpc_fallback"] {
        metrics::counter!(BLOCK_TIME_LOOKUPS_TOTAL, "source" => source).increment(0);
    }

    log::info!("metrics listener started on http://{addr}/metrics");
    Ok(())
}

/// Bridges `carbon_core`'s metrics trait onto the `metrics` facade.
///
/// carbon hands us borrowed, dynamically-chosen names, so each call allocates a `String` for
/// the key. At this program's update rate (a handful of transactions a day) that is free; if
/// this indexer is ever pointed at a hot program, cache `metrics::Counter` handles in a
/// `DashMap<String, Counter>` here instead.
#[derive(Debug, Default)]
pub struct PrometheusMetrics;

#[async_trait]
impl Metrics for PrometheusMetrics {
    async fn initialize(&self) -> CarbonResult<()> {
        Ok(())
    }

    /// No-op: the Prometheus exporter is pull-based, so there is nothing to flush. The
    /// pipeline calls this on `metrics_flush_interval`.
    async fn flush(&self) -> CarbonResult<()> {
        Ok(())
    }

    async fn shutdown(&self) -> CarbonResult<()> {
        Ok(())
    }

    async fn update_gauge(&self, name: &str, value: f64) -> CarbonResult<()> {
        metrics::gauge!(name.to_string()).set(value);
        Ok(())
    }

    async fn increment_counter(&self, name: &str, value: u64) -> CarbonResult<()> {
        metrics::counter!(name.to_string()).increment(value);
        Ok(())
    }

    async fn record_histogram(&self, name: &str, value: f64) -> CarbonResult<()> {
        metrics::histogram!(name.to_string()).record(value);
        Ok(())
    }
}

// --- the indexer's own metrics -------------------------------------------------------------

/// One committed batch flush.
pub fn record_flush(duration: Duration, rows: usize) {
    metrics::histogram!(DB_FLUSH_DURATION_SECONDS).record(duration.as_secs_f64());
    metrics::histogram!(DB_FLUSH_ROWS).record(rows as f64);
}

/// A decoded instruction that could not be mapped. `reason` is a low-cardinality label
/// (see `mapping::MappingError::reason`), never a signature or a pubkey.
pub fn inc_decode_skipped(reason: &'static str) {
    metrics::counter!(DECODE_SKIPPED_TOTAL, "reason" => reason).increment(1);
}

pub fn inc_grpc_reconnect() {
    inc_grpc_reconnect_by(1);
}

fn inc_grpc_reconnect_by(n: u64) {
    metrics::counter!(GRPC_RECONNECTS_TOTAL).increment(n);
}

/// `source` is one of `stream`, `cache`, `rpc`, `rpc_fallback`.
pub fn inc_block_time_lookup(source: &'static str) {
    metrics::counter!(BLOCK_TIME_LOOKUPS_TOTAL, "source" => source).increment(1);
}
