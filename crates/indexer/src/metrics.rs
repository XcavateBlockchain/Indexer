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
//! If [`install`] is never called (unit tests, a `backfill`/`snapshot` run without `--metrics`)
//! the
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
/// Gauge: oldest slot the running history walk has committed.
pub const BACKFILL_LAST_PROCESSED_SLOT: &str = "backfill_last_processed_slot";
/// Counter: signatures returned by `getSignaturesForAddress` across every crawl.
pub const BACKFILL_SIGNATURES_FETCHED_TOTAL: &str = "backfill_signatures_fetched_total";
/// Gauge: accounts written by the last `getProgramAccounts` snapshot.
pub const SNAPSHOT_ACCOUNTS_LOADED: &str = "snapshot_accounts_loaded";
/// Gauge: `getSlot` as of the last reconciliation cycle.
pub const CHAIN_TIP_SLOT: &str = "chain_tip_slot";
/// Gauge: `sync_state.last_contiguous_slot`.
///
/// `chain_tip_slot - last_contiguous_slot` is the slot-lag panel: the single number that says
/// whether the indexer is keeping up. It is a *proven-contiguous* lag, not a "last row written"
/// lag -- on this deliberately idle program the two are very different things.
pub const LAST_CONTIGUOUS_SLOT: &str = "last_contiguous_slot";
/// Counter: NEW program-upgrade boundaries committed to `program_upgrades` (ADR-24). Counts
/// first observations only (at most once per boundary -- see `WriteOp::RecordProgramUpgrade`
/// for the ambiguous-commit caveat): crawl re-walks re-deliver historical upgrade
/// transactions, but the batcher bumps this only for rows `ON CONFLICT DO NOTHING` actually
/// inserted. Any increase means a tracked program's bytecode changed under a decoder
/// generated from the pre-upgrade IDL: the ProgramUpgradeDetected alert fires on it; the
/// `program_upgrades` table is the durable record when the alert window is missed.
pub const PROGRAM_UPGRADES_DETECTED_TOTAL: &str = "program_upgrades_detected_total";
/// Counter: off-chain property-metadata fetch attempts by outcome (ADR-27), labelled
/// `result` = `success` / `failure`. A rising `failure` series is the object-storage or
/// URL-shape problem surfacing early; the durable per-asset state is `last_error` in
/// `marketplace_property_metadata`.
pub const PROPERTY_METADATA_FETCH_TOTAL: &str = "property_metadata_fetched_total";
/// Gauge: the property-metadata work-set size after the last fetch cycle (ADR-27): open
/// `PropertyAsset`s whose metadata is missing, stale, or a failure past its backoff. 0 =
/// every asset's metadata is current; a persistently non-zero value means the fetcher is
/// losing to the network (or a bad URI) -- the per-asset reason is `last_error`.
pub const PROPERTY_METADATA_PENDING: &str = "property_metadata_pending";
/// Counter: outbound webhook deliveries by outcome (ADR-28), labelled `result` =
/// `success` / `failure`. A rising `failure` series (with `webhooks_pending` climbing) is the
/// endpoint-or-network problem surfacing early; the durable per-event state is `last_error`
/// in `webhook_events`.
pub const WEBHOOKS_DELIVERED_TOTAL: &str = "webhooks_delivered_total";
/// Gauge: the webhook delivery work-set size after the last delivery cycle (ADR-28): events
/// recorded but not yet delivered. 0 = every recorded event has been delivered; a
/// persistently non-zero value means the loop is losing to the endpoint (or the network) --
/// the per-event reason is `last_error` in `webhook_events`.
pub const WEBHOOKS_PENDING: &str = "webhooks_pending";
/// Counter: property image mirror attempts by outcome (ADR-31), labelled `result` =
/// `success` / `failure`. A rising `failure` series is the source-host or object-storage
/// problem surfacing early; the durable per-image state is `last_error` in
/// `marketplace_property_image`.
pub const PROPERTY_IMAGES_MIRRORED_TOTAL: &str = "property_images_mirrored_total";
/// Gauge: the property image mirror work-set size after the last mirror cycle (ADR-31):
/// images not yet mirrored (never attempted, failed and past their backoff, or whose source
/// URI changed). 0 = every image has a current thumbnail; a persistently non-zero value
/// means the mirror is losing to the network (or a bad URI) -- the per-image reason is
/// `last_error` in `marketplace_property_image`.
pub const PROPERTY_IMAGES_PENDING: &str = "property_images_pending";

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
    metrics::describe_gauge!(
        BACKFILL_LAST_PROCESSED_SLOT,
        "Oldest slot committed by the running history backfill walk"
    );
    metrics::describe_counter!(
        BACKFILL_SIGNATURES_FETCHED_TOTAL,
        "Signatures returned by getSignaturesForAddress across backfill and reconciliation crawls"
    );
    metrics::describe_gauge!(
        SNAPSHOT_ACCOUNTS_LOADED,
        "Accounts written by the last getProgramAccounts snapshot"
    );
    metrics::describe_gauge!(CHAIN_TIP_SLOT, "Chain tip slot as of the last getSlot");
    metrics::describe_gauge!(
        LAST_CONTIGUOUS_SLOT,
        "sync_state.last_contiguous_slot: no gaps exist below this slot"
    );
    metrics::describe_counter!(
        PROGRAM_UPGRADES_DETECTED_TOTAL,
        "Newly-recorded BPFLoaderUpgradeable upgrades of tracked programs (first observations only)"
    );
    metrics::describe_counter!(
        PROPERTY_METADATA_FETCH_TOTAL,
        "Off-chain property-metadata fetch attempts by outcome (ADR-27)"
    );
    metrics::describe_gauge!(
        PROPERTY_METADATA_PENDING,
        "Property-metadata work-set size after the last fetch cycle: assets awaiting a (re)fetch"
    );
    metrics::describe_counter!(
        WEBHOOKS_DELIVERED_TOTAL,
        "Outbound webhook deliveries by outcome (ADR-28)"
    );
    metrics::describe_gauge!(
        WEBHOOKS_PENDING,
        "Webhook delivery work-set size after the last cycle: events recorded but not yet delivered"
    );
    metrics::describe_counter!(
        PROPERTY_IMAGES_MIRRORED_TOTAL,
        "Property image mirror attempts by outcome (ADR-31): one increment per work-set image per cycle, success or failure. The failure series feeds the PropertyImageMirrorFailing alert."
    );
    metrics::describe_gauge!(
        PROPERTY_IMAGES_PENDING,
        "Property image mirror work-set size after the last cycle (ADR-31): images not yet mirrored (never attempted, failed and past their backoff, or whose source URI changed)"
    );

    // Register every counter (and every label value it can take) at zero. Without this the
    // series simply does not exist until the first occurrence, and a Prometheus rule like
    // `rate(decode_skipped_total[5m]) > 0` reads "no data" rather than "healthy" -- which is
    // exactly backwards for metrics whose whole purpose is to be zero.
    inc_grpc_reconnect_by(0);
    for program in crate::programs::PROGRAMS {
        add_backfill_signatures_fetched(program.name, 0);
        for reason in ["missing_account", "empty_absolute_path", "serialize"] {
            metrics::counter!(DECODE_SKIPPED_TOTAL, "program" => program.name, "reason" => reason)
                .increment(0);
        }
        metrics::counter!(PROGRAM_UPGRADES_DETECTED_TOTAL, "program" => program.name).increment(0);
    }
    for result in ["success", "failure"] {
        metrics::counter!(PROPERTY_METADATA_FETCH_TOTAL, "result" => result).increment(0);
        metrics::counter!(WEBHOOKS_DELIVERED_TOTAL, "result" => result).increment(0);
        metrics::counter!(PROPERTY_IMAGES_MIRRORED_TOTAL, "result" => result).increment(0);
    }
    // WEBHOOKS_PENDING is deliberately not pre-registered (same convention as the slot gauges
    // and property_metadata_pending): an absent series reads as "this process has not run a
    // delivery cycle yet" (which is exactly the case when WEBHOOK_URL is unset), while 0
    // would be indistinguishable from "caught up".
    // PROPERTY_METADATA_PENDING is deliberately not pre-registered: an absent series reads as
    // "this process has not run a fetch cycle yet" (same convention as the slot gauges),
    // while 0 would be indistinguishable from "caught up" only after the first cycle anyway.
    // PROPERTY_IMAGES_PENDING is deliberately not pre-registered too: an absent series reads
    // as "this process has not run a mirror cycle yet" (exactly the case when OBJECT_STORAGE_*
    // is unset and the mirror is disabled).
    for source in ["stream", "cache", "rpc", "rpc_fallback"] {
        metrics::counter!(BLOCK_TIME_LOOKUPS_TOTAL, "source" => source).increment(0);
    }
    // The four slot/count gauges are deliberately NOT pre-registered at zero: a slot gauge
    // reading 0 is indistinguishable from a real (catastrophic) value, whereas an absent series
    // reads as "this process has not measured it yet", which is the truth until the first
    // reconciliation cycle or snapshot runs.

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

/// A decoded instruction that could not be mapped. `program` is the registry name and
/// `reason` a low-cardinality label (see `mapping::MappingError::reason`) -- never a
/// signature or a pubkey.
pub fn inc_decode_skipped(program: &'static str, reason: &'static str) {
    metrics::counter!(DECODE_SKIPPED_TOTAL, "program" => program, "reason" => reason).increment(1);
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

/// Oldest slot one program's running history walk has committed (it walks downwards, so this
/// falls). Labelled by program: the walks are independent.
pub fn set_backfill_last_processed_slot(program: &'static str, slot: u64) {
    metrics::gauge!(BACKFILL_LAST_PROCESSED_SLOT, "program" => program).set(slot as f64);
}

/// Signatures returned by one `getSignaturesForAddress` page of one program's crawl.
pub fn add_backfill_signatures_fetched(program: &'static str, n: u64) {
    metrics::counter!(BACKFILL_SIGNATURES_FETCHED_TOTAL, "program" => program).increment(n);
}

pub fn set_snapshot_accounts_loaded(program: &'static str, n: u64) {
    metrics::gauge!(SNAPSHOT_ACCOUNTS_LOADED, "program" => program).set(n as f64);
}

pub fn set_chain_tip_slot(slot: u64) {
    metrics::gauge!(CHAIN_TIP_SLOT).set(slot as f64);
}

/// One program's `sync_state.last_contiguous_slot`. The operator lag panel/alert uses
/// `chain_tip_slot - min(last_contiguous_slot)`: the fleet is only as caught-up as its
/// laggiest program.
pub fn set_last_contiguous_slot(program: &'static str, slot: u64) {
    metrics::gauge!(LAST_CONTIGUOUS_SLOT, "program" => program).set(slot as f64);
}

/// One NEWLY-recorded upgrade boundary for `program` (see the constant's docs: first
/// observations only, called by the batcher strictly after the row's commit).
pub fn inc_program_upgrade_detected(program: &'static str) {
    metrics::counter!(PROGRAM_UPGRADES_DETECTED_TOTAL, "program" => program).increment(1);
}

/// One off-chain property-metadata fetch attempt (ADR-27). `result` is `success` or
/// `failure` (low-cardinality label -- never a URI or an error message).
pub fn inc_property_metadata_fetch(result: &'static str) {
    metrics::counter!(PROPERTY_METADATA_FETCH_TOTAL, "result" => result).increment(1);
}

/// The property-metadata work-set size after one fetch cycle (ADR-27).
pub fn set_property_metadata_pending(n: i64) {
    metrics::gauge!(PROPERTY_METADATA_PENDING).set(n as f64);
}

/// One outbound webhook delivery attempt (ADR-28). `result` is `success` or `failure`
/// (low-cardinality label -- never a URL, an event id, or an error message).
pub fn inc_webhook_delivery(result: &'static str) {
    metrics::counter!(WEBHOOKS_DELIVERED_TOTAL, "result" => result).increment(1);
}

/// The webhook delivery work-set size after one delivery cycle (ADR-28).
pub fn set_webhooks_pending(n: i64) {
    metrics::gauge!(WEBHOOKS_PENDING).set(n as f64);
}

/// One property image mirror attempt (ADR-31). `result` is `success` or `failure`
/// (low-cardinality label -- never a URI or an error message; the per-image reason is
/// `last_error` in `marketplace_property_image`).
pub fn inc_property_image_mirror(result: &'static str) {
    metrics::counter!(PROPERTY_IMAGES_MIRRORED_TOTAL, "result" => result).increment(1);
}

/// The property image mirror work-set size after one mirror cycle (ADR-31).
pub fn set_property_images_pending(n: i64) {
    metrics::gauge!(PROPERTY_IMAGES_PENDING).set(n as f64);
}
