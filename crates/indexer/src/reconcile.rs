//! The reconciliation supervisor: the only thing that advances any program's
//! `sync_state.last_contiguous_slot`.
//!
//! ## The division of labour (read this before wondering why a crawler runs next to a stream)
//!
//! * The **live Yellowstone stream** provides FRESHNESS. A new transaction is in the database
//!   within a second of landing on chain.
//! * The **periodic crawl** provides COMPLETENESS. It is authoritative about "nothing below
//!   slot T is missing", which the stream can never be: carbon's Yellowstone datasource
//!   re-subscribes internally on error (and swallows auth/plan rejections in a retry loop), so a
//!   process cannot tell a healthy idle stream from a broken one. On these programs -- idle for
//!   days at a time -- "no updates" is the normal case, so silence proves nothing either way.
//!
//! Hence: `last_contiguous_slot` is advanced ONLY here, and only by evidence this task gathered
//! itself. With five programs, each has its own row, frontier, and crawl -- one program's lag
//! (say, an unfinished backfill) must never freeze the others' frontiers.
//!
//! ## One cycle (per interval tick)
//!
//! 1. `T = getSlot(confirmed)`, recorded **before** any crawl and shared by every program's
//!    cycle this tick, so each claim we end up making ("nothing below T is missing for
//!    program P") is only ever about a range that program's crawl actually walked.
//! 2. Per program: crawl `getSignaturesForAddress` newest -> oldest until a signature at or
//!    below that program's current `last_contiguous_slot` shows up, re-writing everything
//!    above it. On a quiet program that is one page and zero `getTransaction` calls beyond
//!    what is already indexed; every write is idempotent, so a re-walk changes zero rows.
//! 3. Once a program's crawl rows are committed, advance its `last_contiguous_slot` to T.
//!
//! ## Cost
//!
//! One `getSlot` + one `getSignaturesForAddress` page per program per RECONCILE_INTERVAL
//! (default 300 s) = ~2,880 requests/day for five programs, plus one `getTransaction` per
//! genuinely new transaction. Alchemy's free tier is 100 M compute units/month;
//! `getSignaturesForAddress` is 67 CU and `getSlot` 10 CU, so this is ~5.8 M CU/month, under
//! 6 % of the budget. The live stream, not this loop, is what keeps latency low, so the
//! interval can be raised freely if that budget ever tightens.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::batcher;
use crate::block_time::BlockTimeResolver;
use crate::config::{redact_key, Config};
use crate::crawl::{self, CrawlDeps, CrawlRequest};
use crate::db;
use crate::metrics::PrometheusMetrics;
use crate::processors::TrackedAccounts;
use crate::programs::ProgramSpec;
use crate::sync_frontier::SyncFrontier;

/// Page size for a reconciliation crawl. Small on purpose: a healthy cycle finds one or two new
/// signatures, and the page only needs to reach back past `last_contiguous_slot`. A cycle after
/// a long outage simply uses several pages.
const RECONCILE_PAGE_SIZE: usize = 50;

/// Per-window delivery timeout inside a cycle (see [`crate::backfill::DEFAULT_WINDOW_IDLE_TIMEOUT`]).
const WINDOW_IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// The per-program sync frontiers, keyed by program id. Built once at startup; the stream
/// reconnect loop opens every frontier's gap (one shared stream dropping is a potential gap
/// for all of them), and each program's backfill/reconcile closes only its own.
pub type Frontiers = HashMap<Pubkey, Arc<SyncFrontier>>;

/// Run reconciliation cycles for every configured program until `shutdown` fires.
pub async fn supervise(
    cfg: &Config,
    pool: &PgPool,
    frontiers: &Frontiers,
    block_time: &Arc<BlockTimeResolver>,
    tracked: &TrackedAccounts,
    interval: Duration,
    shutdown: CancellationToken,
) {
    log::info!(
        "reconciliation supervisor started (every {interval:?}, programs: {}); it is the only \
         writer of sync_state.last_contiguous_slot",
        cfg.programs
            .iter()
            .map(|p| p.name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    loop {
        // One tip read serves every program's cycle this tick.
        match read_tip(cfg).await {
            Ok(tip) => {
                crate::metrics::set_chain_tip_slot(tip);
                for program in &cfg.programs {
                    if shutdown.is_cancelled() {
                        break;
                    }
                    let Some(frontier) = frontiers.get(&program.id) else {
                        log::error!(
                            "reconcile[{}]: no frontier registered; skipping",
                            program.name
                        );
                        continue;
                    };
                    match cycle(
                        cfg,
                        program,
                        pool,
                        frontier,
                        block_time,
                        tracked,
                        tip,
                        shutdown.clone(),
                    )
                    .await
                    {
                        // `cycle` logs its own outcome; nothing to add here.
                        Ok(_) => {}
                        // A failed cycle is not fatal: the next one re-walks the same range
                        // (nothing was advanced, so the range only grows) and the live stream
                        // keeps the data fresh meanwhile.
                        //
                        // `{e:#}` walks the whole anyhow context chain, which can include a
                        // crawl failure against the keyed Alchemy RPC endpoint (see
                        // crate::config::redact_key) -- redact before logging.
                        Err(e) => log::error!(
                            "reconcile[{}] cycle failed (will retry next interval): {}",
                            program.name,
                            redact_key(&format!("{e:#}"))
                        ),
                    }
                }
            }
            Err(e) => log::error!(
                "reconcile: getSlot failed, skipping this tick: {}",
                redact_key(&format!("{e:#}"))
            ),
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

/// `getSlot(confirmed)`, tried on each endpoint in turn: a tick that cannot read the tip
/// cannot advance anything.
async fn read_tip(cfg: &Config) -> Result<u64> {
    let mut tip = Err(anyhow::anyhow!("reconcile: no RPC endpoint configured"));
    for url in cfg.rpc_endpoints() {
        let rpc = RpcClient::new_with_commitment(url, CommitmentConfig::confirmed());
        tip = rpc
            .get_slot_with_commitment(CommitmentConfig::confirmed())
            .await
            .context("reconcile: getSlot failed");
        if tip.is_ok() {
            break;
        }
    }
    tip
}

/// One program's cycle against a pre-read tip. Returns the slot its `last_contiguous_slot`
/// was advanced to, if it was.
#[allow(clippy::too_many_arguments)]
pub async fn cycle(
    cfg: &Config,
    program: &'static ProgramSpec,
    pool: &PgPool,
    frontier: &Arc<SyncFrontier>,
    block_time: &Arc<BlockTimeResolver>,
    tracked: &TrackedAccounts,
    tip: u64,
    shutdown: CancellationToken,
) -> Result<Option<i64>> {
    let rpc_urls = cfg.rpc_endpoints();
    let program_id = program.id.to_bytes().to_vec();

    let state = db::sync_state::get_sync_state(pool, &program_id)
        .await
        .context("reconcile: reading sync_state")?
        .with_context(|| format!("reconcile: sync_state row missing for {}", program.name))?;
    crate::metrics::set_last_contiguous_slot(
        program.name,
        state.last_contiguous_slot.max(0) as u64,
    );

    if !state.backfill_complete {
        log::info!(
            "reconcile[{}]: skipping (backfill has not completed; last_contiguous_slot {} stays \
             put, chain tip {tip})",
            program.name,
            state.last_contiguous_slot
        );
        return Ok(None);
    }

    // The database is the authority on backfill completion; the in-memory frontier flag is a
    // mirror seeded at startup. A backfill completed by ANOTHER process (the documented
    // remedy when a startup backfill job fails: `indexer backfill`, possibly --program'd,
    // alongside the live indexer) flips the DB flag but cannot reach this process's atomic --
    // without this re-sync, every cycle would crawl the full range and then refuse to
    // advance at `may_advance()`, forever, until an undocumented restart.
    if !frontier.backfill_complete() {
        log::info!(
            "reconcile[{}]: sync_state.backfill_complete is true but this process's frontier \
             still says false (backfill completed by another process); re-syncing the frontier \
             from the database",
            program.name
        );
        frontier.set_backfill_complete(true);
    }

    let low = state.last_contiguous_slot.max(0) as u64;
    if tip <= low {
        // The RPC's tip is behind what we already claim (different node, or a lagging replica).
        // Nothing to do; advancing would be a claim about slots we did not walk.
        log::debug!(
            "reconcile[{}]: chain tip {tip} is not ahead of last_contiguous_slot {low}",
            program.name
        );
        return Ok(None);
    }

    // Own batcher: dropping it and awaiting the flusher is the commit barrier that makes it safe
    // to advance the frontier afterwards.
    let (bat, flusher) = batcher::spawn(pool.clone(), shutdown.clone());
    let outcome = crawl::crawl(
        CrawlRequest {
            program,
            // About to claim "no gaps below tip": every enumeration page must come from a
            // node whose confirmed view has reached the tip (see CrawlRequest).
            min_page_view_slot: Some(tip),
            rpc_urls: &rpc_urls,
            // Everything strictly above `last_contiguous_slot`; that slot itself is already
            // covered by definition.
            stop_below: low + 1,
            start_before: None,
            page_size: RECONCILE_PAGE_SIZE,
            // The reconciler always starts from the tip, so a resume cursor would be noise --
            // and would fight with the history backfill over this program's cursor row.
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
            programs: &cfg.programs,
        },
        shutdown.clone(),
    )
    .await;

    drop(bat);
    // FINDING 2 (Task-4 fix round): awaiting the flusher is a commit barrier only if it reports
    // one. A panicked flusher task is indistinguishable from a dropped batch here, so it is
    // treated the same conservative way.
    let flush_outcome = flusher.await.unwrap_or(batcher::FlushOutcome::OpsDropped);
    let summary = outcome?;

    finish(
        pool,
        program,
        frontier,
        flush_outcome,
        low,
        tip,
        summary.signatures_expected,
        summary.windows,
    )
    .await
}

/// Applies (or skips) the end-of-cycle completion effects for one program: closing its sync
/// gap and advancing its `last_contiguous_slot` to `tip`. Split out of `cycle` so FINDING 2
/// (Task-4 fix round) is unit-testable without a live RPC crawl: if `flush_outcome` says the
/// batcher had to drop a batch (a double fault -- a commit kept failing and shutdown fired
/// during its retry backoff), the crawl's summary is not evidence that `[low+1, tip]` is
/// actually in the database, so neither the gap-close nor the advance may happen -- the next
/// cycle re-walks the same range (idempotent) and retries.
#[allow(clippy::too_many_arguments)]
async fn finish(
    pool: &PgPool,
    program: &'static ProgramSpec,
    frontier: &SyncFrontier,
    flush_outcome: batcher::FlushOutcome,
    low: u64,
    tip: u64,
    signatures_expected: u64,
    windows: usize,
) -> Result<Option<i64>> {
    if !flush_outcome.all_committed() {
        log::warn!(
            "reconcile[{}]: PARTIAL CYCLE -- write op(s) from this crawl were dropped uncommitted \
             during a double fault (DB commit failure + shutdown); refusing to close the sync \
             gap or advance last_contiguous_slot to {tip}, since some of the rows this cycle \
             claims to have re-covered may be missing. The next cycle re-walks the same range \
             (idempotent) and will retry.",
            program.name
        );
        return Ok(None);
    }

    // The crawl covered [low+1, tip] in full, so any hole a stream outage left in that range has
    // just been filled.
    frontier.gap_closed();
    if !frontier.may_advance() {
        log::warn!(
            "reconcile[{}]: crawl completed but the frontier still refuses to advance",
            program.name
        );
        return Ok(None);
    }

    let advanced =
        db::sync_state::advance_last_contiguous_slot(pool, &program.id.to_bytes(), tip as i64)
            .await
            .context("reconcile: advancing last_contiguous_slot")?;
    crate::metrics::set_last_contiguous_slot(program.name, tip);
    log::info!(
        "reconcile[{}]: crawled {signatures_expected} signature(s) in {windows} window(s) above \
         slot {low}; last_contiguous_slot -> {tip}",
        program.name,
    );

    Ok(if advanced.rows_affected() > 0 {
        Some(tip as i64)
    } else {
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batcher::FlushOutcome;
    use crate::programs;

    fn test_program() -> &'static ProgramSpec {
        programs::by_name("xcavate_whitelist").expect("registry has the whitelist")
    }

    // --- FINDING 2: dropped ops must skip both the gap-close and the advance ------------------

    #[sqlx::test(migrations = "../../migrations")]
    async fn dropped_ops_skip_the_gap_close_and_the_advance(pool: PgPool) {
        let program = test_program();
        let pid = program.id.to_bytes().to_vec();
        db::sync_state::init_sync_state(&pool, &pid, 100)
            .await
            .unwrap();
        // backfill_complete = true, but a fresh frontier starts with gap_open = true; that is
        // exactly the pre-cycle state this test needs to see stay frozen.
        let frontier = SyncFrontier::new(true);

        let result = finish(
            &pool,
            program,
            &frontier,
            FlushOutcome::OpsDropped,
            100,
            500,
            3,
            1,
        )
        .await
        .expect("a dropped-ops cycle must not error, only skip");
        assert_eq!(result, None);
        assert!(
            frontier.gap_open(),
            "the gap must stay open when the flush dropped ops"
        );

        let state = db::sync_state::get_sync_state(&pool, &pid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state.last_contiguous_slot, 100,
            "last_contiguous_slot must not advance"
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_clean_flush_closes_the_gap_and_advances(pool: PgPool) {
        let program = test_program();
        let pid = program.id.to_bytes().to_vec();
        db::sync_state::init_sync_state(&pool, &pid, 100)
            .await
            .unwrap();
        let frontier = SyncFrontier::new(true);

        let result = finish(
            &pool,
            program,
            &frontier,
            FlushOutcome::AllCommitted,
            100,
            500,
            3,
            1,
        )
        .await
        .unwrap();
        assert_eq!(result, Some(500));
        assert!(!frontier.gap_open());

        let state = db::sync_state::get_sync_state(&pool, &pid)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.last_contiguous_slot, 500);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn one_programs_advance_does_not_touch_anothers_row(pool: PgPool) {
        let whitelist = test_program();
        let marketplace = programs::by_name("marketplace").unwrap();
        db::sync_state::init_sync_state(&pool, &whitelist.id.to_bytes(), 100)
            .await
            .unwrap();
        db::sync_state::init_sync_state(&pool, &marketplace.id.to_bytes(), 200)
            .await
            .unwrap();

        let frontier = SyncFrontier::new(true);
        finish(
            &pool,
            whitelist,
            &frontier,
            FlushOutcome::AllCommitted,
            100,
            500,
            1,
            1,
        )
        .await
        .unwrap();

        let other = db::sync_state::get_sync_state(&pool, &marketplace.id.to_bytes())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            other.last_contiguous_slot, 200,
            "the marketplace row must be untouched by the whitelist's advance"
        );
    }
}
