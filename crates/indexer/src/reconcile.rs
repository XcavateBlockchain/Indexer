//! The reconciliation supervisor: the only thing that advances
//! `sync_state.last_contiguous_slot`.
//!
//! ## The division of labour (read this before wondering why a crawler runs next to a stream)
//!
//! * The **live Yellowstone stream** provides FRESHNESS. A new whitelist transaction is in the
//!   database within a second of landing on chain.
//! * The **periodic crawl** provides COMPLETENESS. It is authoritative about "nothing below
//!   slot T is missing", which the stream can never be: carbon's Yellowstone datasource
//!   re-subscribes internally on error (and swallows auth/plan rejections in a retry loop), so a
//!   process cannot tell a healthy idle stream from a broken one. On this program -- idle for
//!   days at a time -- "no updates" is the normal case, so silence proves nothing either way.
//!
//! Hence: `last_contiguous_slot` is advanced ONLY here, and only by evidence this task gathered
//! itself.
//!
//! ## One cycle
//!
//! 1. `T = getSlot(confirmed)`, recorded **before** the crawl, so the claim we end up making
//!    ("nothing below T is missing") is only ever about a range we actually walked.
//! 2. Crawl `getSignaturesForAddress` newest -> oldest until a signature at or below the current
//!    `last_contiguous_slot` shows up, re-writing everything above it. On a quiet program that
//!    is one page and zero `getTransaction` calls beyond what is already indexed; every write is
//!    idempotent, so a re-walk changes zero rows.
//! 3. Once the crawl's rows are committed, advance `last_contiguous_slot` to T.
//!
//! ## Cost
//!
//! One `getSlot` + one `getSignaturesForAddress` page per RECONCILE_INTERVAL (default 300 s) =
//! ~576 requests/day, plus one `getTransaction` per genuinely new transaction. Alchemy's free
//! tier is 100 M compute units/month; `getSignaturesForAddress` is 67 CU and `getSlot` 10 CU, so
//! this is ~1.4 M CU/month, under 2 % of the budget. The live stream, not this loop, is what
//! keeps latency low, so the interval can be raised freely if that budget ever tightens.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::batcher;
use crate::block_time::BlockTimeResolver;
use crate::config::Config;
use crate::crawl::{self, CrawlDeps, CrawlRequest};
use crate::db;
use crate::metrics::PrometheusMetrics;
use crate::processors::TrackedAccounts;
use crate::sync_frontier::SyncFrontier;

/// Page size for a reconciliation crawl. Small on purpose: a healthy cycle finds one or two new
/// signatures, and the page only needs to reach back past `last_contiguous_slot`. A cycle after
/// a long outage simply uses several pages.
const RECONCILE_PAGE_SIZE: usize = 50;

/// Per-window delivery timeout inside a cycle (see [`crate::backfill::DEFAULT_WINDOW_IDLE_TIMEOUT`]).
const WINDOW_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// Run reconciliation cycles until `shutdown` fires.
pub async fn supervise(
    cfg: &Config,
    pool: &PgPool,
    frontier: &Arc<SyncFrontier>,
    block_time: &Arc<BlockTimeResolver>,
    tracked: &TrackedAccounts,
    interval: Duration,
    shutdown: CancellationToken,
) {
    log::info!(
        "reconciliation supervisor started (every {interval:?}); it is the only writer of \
         sync_state.last_contiguous_slot"
    );
    loop {
        match cycle(cfg, pool, frontier, block_time, tracked, shutdown.clone()).await {
            // `cycle` logs its own outcome; nothing to add here.
            Ok(_) => {}
            // A failed cycle is not fatal: the next one re-walks the same range (nothing was
            // advanced, so the range only grows) and the live stream keeps the data fresh
            // meanwhile.
            Err(e) => log::error!("reconcile cycle failed (will retry next interval): {e:#}"),
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.cancelled() => {
                log::info!("reconciliation supervisor stopping");
                return;
            }
        }
    }
}

/// One cycle. Returns the slot `last_contiguous_slot` was advanced to, if it was.
pub async fn cycle(
    cfg: &Config,
    pool: &PgPool,
    frontier: &Arc<SyncFrontier>,
    block_time: &Arc<BlockTimeResolver>,
    tracked: &TrackedAccounts,
    shutdown: CancellationToken,
) -> Result<Option<i64>> {
    let rpc_urls = cfg.rpc_endpoints();

    // T, recorded before the crawl (see the module docs). Tried on each endpoint in turn: a
    // cycle that cannot read the tip cannot advance anything.
    let mut tip = Err(anyhow::anyhow!("reconcile: no RPC endpoint configured"));
    for url in &rpc_urls {
        let rpc = RpcClient::new_with_commitment(url.clone(), CommitmentConfig::confirmed());
        tip = rpc
            .get_slot_with_commitment(CommitmentConfig::confirmed())
            .await
            .context("reconcile: getSlot failed");
        if tip.is_ok() {
            break;
        }
    }
    let tip = tip?;
    crate::metrics::set_chain_tip_slot(tip);

    let state = db::sync_state::get_sync_state(pool)
        .await
        .context("reconcile: reading sync_state")?
        .context("reconcile: sync_state row missing")?;
    crate::metrics::set_last_contiguous_slot(state.last_contiguous_slot as u64);

    if !state.backfill_complete {
        log::info!(
            "reconcile: skipping (backfill has not completed; last_contiguous_slot {} stays put, \
             chain tip {tip})",
            state.last_contiguous_slot
        );
        return Ok(None);
    }

    let low = state.last_contiguous_slot.max(0) as u64;
    if tip <= low {
        // The RPC's tip is behind what we already claim (different node, or a lagging replica).
        // Nothing to do; advancing would be a claim about slots we did not walk.
        log::debug!("reconcile: chain tip {tip} is not ahead of last_contiguous_slot {low}");
        return Ok(None);
    }

    // Own batcher: dropping it and awaiting the flusher is the commit barrier that makes it safe
    // to advance the frontier afterwards.
    let (bat, flusher) = batcher::spawn(pool.clone(), shutdown.clone());
    let outcome = crawl::crawl(
        cfg,
        CrawlRequest {
            rpc_urls: &rpc_urls,
            // Everything strictly above `last_contiguous_slot`; that slot itself is already
            // covered by definition.
            stop_below: low + 1,
            start_before: None,
            page_size: RECONCILE_PAGE_SIZE,
            // The reconciler always starts from the tip, so a resume cursor would be noise --
            // and would fight with the history backfill over the same row.
            persist_cursor: false,
            report_progress: false,
            window_idle_timeout: WINDOW_IDLE_TIMEOUT,
            label: "reconcile",
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

    // The crawl covered [low+1, tip] in full, so any hole a stream outage left in that range has
    // just been filled.
    frontier.gap_closed();
    if !frontier.may_advance() {
        log::warn!("reconcile: crawl completed but the frontier still refuses to advance");
        return Ok(None);
    }

    let advanced = db::sync_state::advance_last_contiguous_slot(pool, tip as i64)
        .await
        .context("reconcile: advancing last_contiguous_slot")?;
    crate::metrics::set_last_contiguous_slot(tip);
    log::info!(
        "reconcile: crawled {} signature(s) in {} window(s) above slot {low}; last_contiguous_slot -> {tip}",
        summary.signatures_expected,
        summary.windows,
    );

    Ok(if advanced.rows_affected() > 0 {
        Some(tip as i64)
    } else {
        None
    })
}
