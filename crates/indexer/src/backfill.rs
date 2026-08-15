//! The resumable history backfill (spec §7, step 4).
//!
//! Walks `getSignaturesForAddress(PROGRAM_ID)` newest -> oldest, from the chain tip (or from a
//! saved cursor) down to `backfill_floor_slot` -- the slot the program was deployed at, below
//! which there is nothing to index -- feeding every transaction through the same instruction
//! pipe and the same batcher as the live stream. On reaching the floor it sets
//! `sync_state.backfill_complete` and closes the sync frontier's gap, which is what allows
//! `last_contiguous_slot` to start advancing at all.
//!
//! ## Resumability
//!
//! It will be interrupted -- it is a long RPC walk against a throttling free tier. Two
//! independent things make that safe:
//!
//! * **Every write is idempotent and slot-guarded** (Task 2), so re-processing a transaction
//!   changes zero rows. Correctness never depends on where the previous run stopped.
//! * **The cursor** (`backfill_cursor`, one row) records the oldest signature whose whole page
//!   has been committed, and a resumed run passes it as `before`. That is purely an RPC-budget
//!   optimisation on top of the first point.
//!
//! The cursor is written *through the batcher*, sorted after the rows of its page, so it can
//! never claim a page whose rows did not commit. It is deleted when the walk reaches its stop
//! condition, so "a cursor exists" means exactly "an interrupted walk is waiting to be
//! resumed": re-running a *completed* backfill therefore re-walks the whole range from the tip
//! and re-verifies it (changing zero rows), rather than exiting as a no-op.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use solana_signature::Signature;
use sqlx::PgPool;
use std::str::FromStr;
use tokio_util::sync::CancellationToken;

use crate::batcher;
use crate::block_time::BlockTimeResolver;
use crate::config::Config;
use crate::crawl::{self, CrawlDeps, CrawlRequest, CrawlSummary, StopReason};
use crate::db;
use crate::metrics::PrometheusMetrics;
use crate::processors::TrackedAccounts;
use crate::sync_frontier::SyncFrontier;

/// Default `getSignaturesForAddress` page size. 100 is the crawler's own default and well
/// inside every provider's limit; the CLI can lower it (useful for exercising resume).
pub const DEFAULT_PAGE_SIZE: usize = 100;

/// How long one window may go without a delivery before the walk is declared stuck. Generous:
/// a throttled free-tier `getTransaction` retries with backoff for ~7 s before carbon gives up
/// on it.
pub const DEFAULT_WINDOW_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

pub struct BackfillOptions {
    /// Exclusive floor. Defaults to `sync_state.backfill_floor_slot`.
    pub floor: Option<u64>,
    pub page_size: usize,
    pub window_idle_timeout: Duration,
}

impl Default for BackfillOptions {
    fn default() -> Self {
        Self {
            floor: None,
            page_size: DEFAULT_PAGE_SIZE,
            window_idle_timeout: DEFAULT_WINDOW_IDLE_TIMEOUT,
        }
    }
}

/// Run the history walk to completion. Safe to call repeatedly and concurrently with the live
/// pipeline.
pub async fn run(
    cfg: &Config,
    pool: &PgPool,
    frontier: &Arc<SyncFrontier>,
    block_time: &Arc<BlockTimeResolver>,
    tracked: &TrackedAccounts,
    opts: BackfillOptions,
    shutdown: CancellationToken,
) -> Result<CrawlSummary> {
    let state = db::sync_state::get_sync_state(pool)
        .await
        .context("backfill: reading sync_state")?
        .context("backfill: sync_state row missing (run migrations first)")?;
    let floor = opts.floor.unwrap_or(state.backfill_floor_slot as u64);

    // The cursor is only a resume point for an *unfinished* walk. A backfill that already
    // completed has no cursor (it is deleted on completion), so an explicit re-run starts at the
    // tip and re-verifies the whole range instead of no-op'ing.
    let cursor = db::backfill_cursor::get_cursor(pool)
        .await
        .context("backfill: reading backfill_cursor")?;
    let start_before = match &cursor {
        Some(c) => {
            log::info!(
                "backfill: resuming below signature {} (slot {})",
                c.signature,
                c.slot
            );
            Some(Signature::from_str(&c.signature).with_context(|| {
                format!(
                    "backfill_cursor holds an invalid signature: {}",
                    c.signature
                )
            })?)
        }
        None => {
            log::info!("backfill: starting from the chain tip");
            None
        }
    };

    let rpc_url = cfg.rpc_url();
    log::info!(
        "backfill: walking {} down to floor slot {floor} via {} (page size {})",
        cfg.program_id,
        // Never log the URL itself: the Alchemy JSON-RPC endpoint carries the key in its path.
        if rpc_url == cfg.rpc_fallback_url {
            cfg.rpc_fallback_url.as_str()
        } else {
            "<primary RPC>"
        },
        opts.page_size,
    );

    // Own batcher: dropping it and awaiting the flusher is the commit barrier that makes it safe
    // to set `backfill_complete` afterwards.
    let (bat, flusher) = batcher::spawn(pool.clone(), shutdown.clone());

    let outcome = crawl::crawl(
        cfg,
        CrawlRequest {
            rpc_url: &rpc_url,
            stop_below: floor,
            start_before,
            page_size: opts.page_size,
            persist_cursor: true,
            window_idle_timeout: opts.window_idle_timeout,
            label: "backfill",
        },
        CrawlDeps {
            batcher: &bat,
            block_time,
            tracked,
            metrics: Arc::new(PrometheusMetrics),
        },
        shutdown.clone(),
    )
    .await;

    drop(bat);
    flusher.await.ok();

    let summary = outcome?;
    log::info!(
        "backfill: walk finished ({:?}) -- {} window(s), {} signature(s) enumerated, {} indexed, \
         {} failed-on-chain and skipped, slots {:?}..={:?}",
        summary.stop,
        summary.windows,
        summary.signatures_enumerated,
        summary.signatures_expected,
        summary.signatures_failed,
        summary.oldest_slot,
        summary.newest_slot,
    );

    // Only now, with every row committed: the walk reached the floor (or the beginning of
    // history, which is the same statement about completeness), so everything from the floor
    // upwards is in the database.
    debug_assert!(matches!(
        summary.stop,
        Some(StopReason::ReachedFloor) | Some(StopReason::HistoryExhausted)
    ));
    db::sync_state::set_backfill_complete(pool, true)
        .await
        .context("backfill: setting sync_state.backfill_complete")?;
    db::backfill_cursor::clear_cursor(pool)
        .await
        .context("backfill: clearing backfill_cursor")?;
    frontier.set_backfill_complete(true);
    // The frontier hook Task 3 left: until this fires, the reconciliation supervisor refuses to
    // advance `last_contiguous_slot`, because everything below the first indexed slot might have
    // been missing.
    frontier.gap_closed();
    log::info!(
        "backfill: sync_state.backfill_complete = true; last_contiguous_slot may now advance"
    );

    Ok(summary)
}
