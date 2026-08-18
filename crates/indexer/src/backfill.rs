//! The resumable history backfill (spec §7, step 4), one walk per program.
//!
//! Walks `getSignaturesForAddress(<program>)` newest -> oldest, from the chain tip (or from
//! that program's saved cursor) down to its `backfill_floor_slot` -- the slot the program was
//! deployed at, below which there is nothing to index -- feeding every transaction through
//! the same instruction pipes and the same batcher as the live stream. On reaching the floor
//! it sets that program's `sync_state.backfill_complete` and closes its sync frontier's gap,
//! which is what allows its `last_contiguous_slot` to start advancing at all.
//!
//! ## Resumability
//!
//! It will be interrupted -- it is a long RPC walk against a throttling free tier. Two
//! independent things make that safe:
//!
//! * **Every write is idempotent and slot-guarded** (Task 2), so re-processing a transaction
//!   changes zero rows. Correctness never depends on where the previous run stopped.
//! * **The cursor** (`backfill_cursor`, one row per program) records the oldest signature
//!   whose whole page has been committed, and a resumed run passes it as `before`. That is
//!   purely an RPC-budget optimisation on top of the first point.
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
use crate::programs::ProgramSpec;
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

/// Run one program's history walk to completion. Safe to call repeatedly and concurrently
/// with the live pipeline; `frontier` must be that program's frontier.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    cfg: &Config,
    program: &'static ProgramSpec,
    pool: &PgPool,
    frontier: &Arc<SyncFrontier>,
    block_time: &Arc<BlockTimeResolver>,
    tracked: &TrackedAccounts,
    opts: BackfillOptions,
    shutdown: CancellationToken,
) -> Result<CrawlSummary> {
    let program_id = program.id.to_bytes().to_vec();
    let state = db::sync_state::get_sync_state(pool, &program_id)
        .await
        .context("backfill: reading sync_state")?
        .with_context(|| {
            format!(
                "backfill: sync_state row missing for {} (run migrations first)",
                program.name
            )
        })?;
    let floor = opts.floor.unwrap_or(state.backfill_floor_slot as u64);

    // The cursor is only a resume point for an *unfinished* walk. A backfill that already
    // completed has no cursor (it is deleted on completion), so an explicit re-run starts at the
    // tip and re-verifies the whole range instead of no-op'ing.
    let cursor = db::backfill_cursor::get_cursor(pool, &program_id)
        .await
        .context("backfill: reading backfill_cursor")?;
    let start_before = match &cursor {
        Some(c) => {
            log::info!(
                "backfill[{}]: resuming below signature {} (slot {})",
                program.name,
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
            log::info!("backfill[{}]: starting from the chain tip", program.name);
            None
        }
    };

    let rpc_urls = cfg.rpc_endpoints();
    log::info!(
        "backfill[{}]: walking {} down to floor slot {floor} via {} (page size {})",
        program.name,
        program.id,
        // Never log the URLs themselves: the Alchemy JSON-RPC endpoint carries the key in its
        // path.
        if rpc_urls.len() > 1 {
            "<primary RPC, public devnet as fallback>"
        } else {
            "<single RPC endpoint>"
        },
        opts.page_size,
    );

    // Own batcher: dropping it and awaiting the flusher is the commit barrier that makes it safe
    // to set `backfill_complete` afterwards.
    let (bat, flusher) = batcher::spawn(pool.clone(), shutdown.clone());

    let outcome = crawl::crawl(
        CrawlRequest {
            program,
            min_page_view_slot: None,
            rpc_urls: &rpc_urls,
            stop_below: floor,
            start_before,
            page_size: opts.page_size,
            persist_cursor: true,
            report_progress: true,
            window_idle_timeout: opts.window_idle_timeout,
            label: "backfill",
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
    log::info!(
        "backfill[{}]: walk finished ({:?}) -- {} window(s), {} signature(s) enumerated, {} indexed, \
         {} failed-on-chain and skipped, slots {:?}..={:?}",
        program.name,
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

    finish(
        pool,
        &program_id,
        frontier,
        flush_outcome,
        floor,
        state.backfill_floor_slot,
    )
    .await?;

    Ok(summary)
}

/// Applies (or skips) the post-walk completion writes. Split out of `run` for two reasons -- the
/// two Important findings from the Task-4 review (see the fix-round report) -- and so both are
/// unit-testable without a live RPC crawl:
///
/// **FINDING 2** (checked first): a crawl finishing is not proof its rows are in the database.
/// If the flusher had to drop a batch (a double fault: a commit kept failing and shutdown fired
/// during its retry backoff, see `crate::batcher::flush`), writing `backfill_complete` or
/// clearing the cursor here would claim completeness for rows that never landed. This is a hard
/// error -- loud, and resumable the same way every other backfill failure is (every write here
/// is idempotent and the cursor, when one exists, only ever advances behind committed rows).
///
/// **FINDING 1**: even with every row committed, an operator-supplied `--floor` above
/// `sync_state.backfill_floor_slot` only walked a *suffix* of history. Setting
/// `backfill_complete` there would unfreeze the reconciliation supervisor (`reconcile.rs`) over
/// the range below the operator's floor that this walk never visited -- exactly the "no gaps
/// below T" lie `last_contiguous_slot` exists to prevent. Unlike Finding 2 this is NOT an error
/// (the operator asked for exactly this range and got it): log a prominent warning and return
/// `Ok(false)` instead. `backfill_cursor` is deliberately left untouched in this branch: the
/// crawl already advanced it (through the batcher, committed with the rows it vouches for) to
/// the oldest signature this partial walk actually reached, which is exactly the resume point a
/// future *unrestricted* `indexer backfill` needs to continue down to the real floor. Clearing
/// it here would discard that progress and force a future full walk to restart from the tip
/// instead of resuming below the operator's floor.
async fn finish(
    pool: &PgPool,
    program_id: &[u8],
    frontier: &SyncFrontier,
    flush_outcome: batcher::FlushOutcome,
    effective_floor: u64,
    backfill_floor_slot: i64,
) -> Result<bool> {
    if !flush_outcome.all_committed() {
        anyhow::bail!(
            "backfill: write op(s) from this walk were dropped uncommitted during a double \
             fault (DB commit failure + shutdown); refusing to set sync_state.backfill_complete \
             or clear backfill_cursor, since some of the rows this walk claims to have indexed \
             may be missing from the database. Re-run `indexer backfill` -- it resumes from its \
             cursor, and every write here is idempotent."
        );
    }

    if effective_floor > backfill_floor_slot.max(0) as u64 {
        log::warn!(
            "backfill: PARTIAL WALK ONLY -- floor {effective_floor} is above \
             sync_state.backfill_floor_slot {backfill_floor_slot}, so this walk covered \
             [{effective_floor}, tip] and NOT the full history down to the program's real \
             floor. sync_state.backfill_complete is left false and last_contiguous_slot stays \
             frozen: completeness is NOT claimed. backfill_cursor is left untouched (it already \
             points at the oldest signature this walk actually committed -- the correct resume \
             point for a future unrestricted `indexer backfill`). Run `indexer backfill` with no \
             --floor (or --floor <= {backfill_floor_slot}) to reach a state where completeness \
             can be claimed."
        );
        return Ok(false);
    }

    db::sync_state::set_backfill_complete(pool, program_id, true)
        .await
        .context("backfill: setting sync_state.backfill_complete")?;
    db::backfill_cursor::clear_cursor(pool, program_id)
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
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batcher::FlushOutcome;

    const PID: &[u8] = &[7u8; 32];

    async fn seeded(pool: &PgPool, backfill_floor_slot: i64) {
        db::sync_state::init_sync_state(pool, PID, backfill_floor_slot)
            .await
            .expect("seed sync_state");
    }

    // --- FINDING 1: an operator floor above the real floor must never claim completeness -----

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_floor_above_the_sync_state_floor_never_claims_completeness(pool: PgPool) {
        seeded(&pool, 100_000).await;
        let frontier = SyncFrontier::new(false);

        let claimed = finish(
            &pool,
            PID,
            &frontier,
            FlushOutcome::AllCommitted,
            500_000,
            100_000,
        )
        .await
        .expect("a partial-floor walk must not error");

        assert!(
            !claimed,
            "an operator floor above the real floor must not claim completeness"
        );

        let state = db::sync_state::get_sync_state(&pool, PID)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !state.backfill_complete,
            "sync_state.backfill_complete must stay false"
        );
        assert!(!frontier.may_advance(), "the frontier must stay frozen");
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_partial_floor_walk_leaves_an_existing_cursor_untouched(pool: PgPool) {
        seeded(&pool, 100_000).await;
        db::backfill_cursor::set_cursor(&pool, PID, "resume-from-here", 250_000)
            .await
            .expect("seed cursor");
        let frontier = SyncFrontier::new(false);

        finish(
            &pool,
            PID,
            &frontier,
            FlushOutcome::AllCommitted,
            500_000,
            100_000,
        )
        .await
        .unwrap();

        let cursor = db::backfill_cursor::get_cursor(&pool, PID)
            .await
            .unwrap()
            .expect("the cursor must survive a partial-floor walk, not be cleared");
        assert_eq!(cursor.signature, "resume-from-here");
        assert_eq!(cursor.slot, 250_000);
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_floor_at_or_below_the_sync_state_floor_claims_completeness(pool: PgPool) {
        seeded(&pool, 100_000).await;
        db::backfill_cursor::set_cursor(&pool, PID, "sig", 100_000)
            .await
            .expect("seed cursor");
        let frontier = SyncFrontier::new(false);

        let claimed = finish(
            &pool,
            PID,
            &frontier,
            FlushOutcome::AllCommitted,
            100_000,
            100_000,
        )
        .await
        .unwrap();
        assert!(claimed);

        let state = db::sync_state::get_sync_state(&pool, PID)
            .await
            .unwrap()
            .unwrap();
        assert!(state.backfill_complete);
        assert!(frontier.may_advance());
        assert!(
            db::backfill_cursor::get_cursor(&pool, PID)
                .await
                .unwrap()
                .is_none(),
            "a genuinely complete walk must clear the cursor"
        );
    }

    // --- FINDING 2: dropped ops must skip every completion write, regardless of the floor -----

    #[sqlx::test(migrations = "../../migrations")]
    async fn dropped_ops_are_a_hard_error_and_claim_nothing(pool: PgPool) {
        seeded(&pool, 100_000).await;
        let frontier = SyncFrontier::new(false);

        let err = finish(
            &pool,
            PID,
            &frontier,
            FlushOutcome::OpsDropped,
            100_000,
            100_000,
        )
        .await
        .expect_err("a double-fault flush must be a hard error");
        assert!(err.to_string().to_lowercase().contains("dropped"));

        let state = db::sync_state::get_sync_state(&pool, PID)
            .await
            .unwrap()
            .unwrap();
        assert!(!state.backfill_complete);
        assert!(!frontier.may_advance(), "the frontier must stay frozen");
    }
}
