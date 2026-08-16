//! Bounded newest -> oldest signature crawls: the completeness half of the indexer.
//!
//! ## Why there is a crawler at all when there is a live stream
//!
//! The Yellowstone gRPC stream gives **freshness** -- a new whitelist transaction shows up in
//! the database within a second. It cannot give **completeness**, because carbon's datasource
//! re-subscribes internally on a stream error (and swallows auth/plan rejections in a retry
//! loop), so the process cannot observe that it missed a window. See [`crate::sync_frontier`].
//!
//! Completeness therefore comes from re-walking `getSignaturesForAddress` and re-writing what
//! it finds. Every write is idempotent and slot-guarded, so a re-walk of an already-indexed
//! range costs RPC calls and changes zero rows. Two callers use this module:
//!
//! * [`crate::backfill`] -- once, downwards, from the tip (or a resume cursor) to
//!   `backfill_floor_slot`.
//! * [`crate::reconcile`] -- forever, every RECONCILE_INTERVAL, from the tip down to
//!   `last_contiguous_slot`. On a quiet program that is a single `getSignaturesForAddress`
//!   page per cycle.
//!
//! ## How one window works
//!
//! carbon's `RpcTransactionCrawler` enumerates signatures itself and never signals "done" -- it
//! polls forever. So this module enumerates *its own* page first (one extra
//! `getSignaturesForAddress` call per page, ~1% of the RPC cost of the page's `getTransaction`
//! calls) and uses it as the plan:
//!
//! 1. [`plan_window`] turns one page into: the signatures the crawler is expected to deliver,
//!    the `until` bound that stops its enumeration at the page boundary, the cursor to persist
//!    afterwards, and whether this page hits a stop condition.
//! 2. The crawler runs with `before`/`until` set to exactly that window, wrapped in [`Observed`]
//!    so every delivered transaction is seen here.
//! 3. When every expected signature has been delivered, the crawler is cancelled; the pipeline
//!    then drains its queue and returns, at which point every transaction in the window has
//!    been mapped and pushed to the batcher.
//! 4. The page's oldest signature is pushed as a `SetBackfillCursor` op, which the batcher
//!    commits *after* the rows it vouches for.
//!
//! Failed transactions are excluded from the expectation: the crawler skips them
//! (`meta.status.is_err()`), matching the old SubQuery handlers, which never saw them either.

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use carbon_core::datasource::{Datasource, DatasourceId, Update, UpdateType};
use carbon_core::error::CarbonResult;
use carbon_core::metrics::MetricsCollection;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_signature::Signature;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::batcher::{Batcher, WriteOp};
use crate::config::{redact_key, Config};
use crate::pipeline::{self, PipeDeps};

/// A transaction the crawler delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub signature: Signature,
    pub slot: u64,
}

pub type ObservationSender = mpsc::UnboundedSender<Observation>;

/// One entry of a `getSignaturesForAddress` page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigInfo {
    pub signature: Signature,
    pub slot: u64,
    /// `err != null` on chain. The crawler skips these, so they are never expected.
    pub failed: bool,
}

/// Why a crawl stopped walking backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// `getSignaturesForAddress` ran out of signatures: we reached the beginning of the
    /// program's history.
    HistoryExhausted,
    /// A signature older than the crawl's floor turned up: everything at or above the floor has
    /// been walked.
    ReachedFloor,
}

/// The `before`/`until` pair (plus page size) handed to one crawler window.
#[derive(Debug, Clone, Copy)]
pub struct CrawlWindow {
    /// Exclusive newer bound: the crawl starts just below this signature. `None` = chain tip.
    pub before: Option<Signature>,
    /// Exclusive older bound: enumeration stops just above this signature. `None` = walk to the
    /// beginning of history.
    pub until: Option<Signature>,
    pub page_size: usize,
}

/// What to do with one page of signatures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowPlan {
    /// Signatures the crawler must deliver before this window counts as complete.
    pub expected: Vec<SigInfo>,
    /// Failed transactions inside the window. The crawler skips them; they are counted and
    /// reported so a signature-set comparison against chain truth can account for them.
    pub failed: Vec<SigInfo>,
    /// `until_signature` for the crawler: the first signature *below* this window, so the
    /// crawler's own enumeration stops exactly where ours did. `None` when the window runs to
    /// the end of history.
    pub until: Option<Signature>,
    /// Cursor to persist once the window has committed: the oldest signature of the window.
    /// `None` when the window ends the walk (there is nothing left to resume).
    pub next_cursor: Option<SigInfo>,
    /// `Some` if this page ends the walk.
    pub stop: Option<StopReason>,
}

/// Plan one window from one page.
///
/// `page` is the result of `getSignaturesForAddress(before = cursor, limit = page_size + 1)`,
/// newest first. The extra entry is a *probe*: it is not part of the window, it is the `until`
/// bound that keeps the crawler from running past it, and its presence is also what
/// distinguishes "there is more history" from "this is the last page".
///
/// `stop_below` is the exclusive floor: a signature is in the window iff `slot >= stop_below`.
/// The history backfill passes `backfill_floor_slot`; the reconciler passes
/// `last_contiguous_slot + 1`.
pub fn plan_window(page: &[SigInfo], stop_below: u64, page_size: usize) -> WindowPlan {
    let split = |window: &[SigInfo]| {
        let (expected, failed): (Vec<_>, Vec<_>) = window.iter().cloned().partition(|s| !s.failed);
        (expected, failed)
    };

    // Nothing at all below `before`: we have walked the program's entire history.
    if page.is_empty() {
        return WindowPlan {
            expected: vec![],
            failed: vec![],
            until: None,
            next_cursor: None,
            stop: Some(StopReason::HistoryExhausted),
        };
    }

    // The page crosses the floor. Take what is at or above it; the first signature below the
    // floor doubles as the crawler's `until` bound, which is exactly the semantics we want:
    // "deliver everything newer than this".
    if let Some(below) = page.iter().position(|s| s.slot < stop_below) {
        let (expected, failed) = split(&page[..below]);
        return WindowPlan {
            expected,
            failed,
            until: Some(page[below].signature),
            next_cursor: None,
            stop: Some(StopReason::ReachedFloor),
        };
    }

    // A full page plus the probe: there is more history below this window.
    if page.len() > page_size {
        let window = &page[..page_size];
        let (expected, failed) = split(window);
        return WindowPlan {
            expected,
            failed,
            until: Some(page[page_size].signature),
            next_cursor: window.last().cloned(),
            stop: None,
        };
    }

    // Short page and no floor crossing: the RPC had nothing more to give.
    let (expected, failed) = split(page);
    WindowPlan {
        expected,
        failed,
        until: None,
        next_cursor: None,
        stop: Some(StopReason::HistoryExhausted),
    }
}

/// A datasource that reports every transaction update it forwards.
///
/// Two jobs, both load-bearing:
///
/// * it tells the caller which signatures have actually been delivered, so a window can be
///   declared complete on evidence rather than on a timeout;
/// * it owns the *inner* datasource's cancellation token, so cancelling a finished window tears
///   down the crawler and closes the pipeline channel -- which makes carbon's `run()` drain its
///   queue and return, instead of breaking mid-queue the way its own cancellation branch does.
pub struct Observed<D> {
    inner: D,
    observer: ObservationSender,
    stop: CancellationToken,
}

impl<D> Observed<D> {
    pub fn new(inner: D, observer: ObservationSender, stop: CancellationToken) -> Self {
        Self {
            inner,
            observer,
            stop,
        }
    }
}

#[async_trait]
impl<D: Datasource + Send + Sync + 'static> Datasource for Observed<D> {
    async fn consume(
        &self,
        id: DatasourceId,
        sender: mpsc::Sender<(Update, DatasourceId)>,
        cancellation_token: CancellationToken,
        metrics: Arc<MetricsCollection>,
    ) -> CarbonResult<()> {
        let (inner_tx, mut inner_rx) = mpsc::channel(sender.max_capacity().max(1));

        // The inner datasource is driven by *our* token, not the pipeline's. The pipeline's
        // token still counts: if carbon cancels it (its own SIGINT handling), we stop too.
        let stop = self.stop.clone();
        let pipeline_token = cancellation_token.clone();
        let stop_for_watch = stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = pipeline_token.cancelled() => stop_for_watch.cancel(),
                _ = stop_for_watch.cancelled() => {}
            }
        });

        self.inner
            .consume(id, inner_tx, stop, metrics)
            .await
            .map_err(|e| {
                carbon_core::error::Error::Custom(format!("crawler datasource failed: {e}"))
            })?;

        let observer = self.observer.clone();
        tokio::spawn(async move {
            while let Some((update, id)) = inner_rx.recv().await {
                if let Update::Transaction(tx) = &update {
                    // A closed receiver just means the caller stopped caring; keep forwarding.
                    let _ = observer.send(Observation {
                        signature: tx.signature,
                        slot: tx.slot,
                    });
                }
                if sender.send((update, id)).await.is_err() {
                    break;
                }
            }
            // Dropping `sender` here is what ends the pipeline: once every datasource sender is
            // gone, carbon's `run()` sees the channel close and returns after processing what it
            // has already received.
        });

        Ok(())
    }

    fn update_types(&self) -> Vec<UpdateType> {
        self.inner.update_types()
    }
}

/// Everything one crawl needs beyond [`Config`].
pub struct CrawlDeps<'a> {
    pub batcher: &'a Batcher,
    pub block_time: &'a Arc<crate::block_time::BlockTimeResolver>,
    pub tracked: &'a crate::processors::TrackedAccounts,
    pub metrics: Arc<dyn carbon_core::metrics::Metrics>,
}

/// One crawl: pages of signatures walked newest -> oldest until a stop condition.
pub struct CrawlRequest<'a> {
    /// Endpoints to try, in order: normally the primary (Alchemy) then the public devnet
    /// fallback. Alchemy's free tier throttles (see MIGRATION_LOG.md), and both the signature
    /// enumeration and the window delivery are idempotent, so failing over and re-doing a page
    /// costs nothing but RPC calls.
    pub rpc_urls: &'a [String],
    /// Exclusive floor: signatures with `slot < stop_below` end the walk and are not indexed.
    pub stop_below: u64,
    /// Resume point: the walk starts just below this signature. `None` = chain tip.
    pub start_before: Option<Signature>,
    pub page_size: usize,
    /// Persist a `backfill_cursor` row after every committed page (the history backfill does;
    /// the reconciler, which always starts from the tip, does not).
    pub persist_cursor: bool,
    /// Publish `backfill_last_processed_slot`. Only the history walk should: it descends
    /// monotonically, so the gauge reads as progress. A reconciliation crawl jumps back to the
    /// tip every cycle and would turn the same gauge into noise.
    pub report_progress: bool,
    /// How long one window may go without a new delivery before it is declared stuck.
    pub window_idle_timeout: Duration,
    /// Label used in log lines, e.g. "backfill" / "reconcile".
    pub label: &'static str,
}

#[derive(Debug, Clone, Default)]
pub struct CrawlSummary {
    pub windows: usize,
    pub signatures_enumerated: u64,
    pub signatures_expected: u64,
    pub signatures_failed: u64,
    pub newest_slot: Option<u64>,
    pub oldest_slot: Option<u64>,
    pub stop: Option<StopReason>,
}

/// Walk the program's signature history from `start_before` down to `stop_below`, pushing every
/// successful transaction through the same pipes the live stream uses.
///
/// Returns once a stop condition is reached. Errors (rather than silently finishing) if a window
/// goes idle with signatures still undelivered -- an incomplete walk must never be mistaken for
/// a finished one, because the caller turns "finished" into `backfill_complete` or into an
/// advance of `last_contiguous_slot`.
pub async fn crawl(
    cfg: &Config,
    req: CrawlRequest<'_>,
    deps: CrawlDeps<'_>,
    shutdown: CancellationToken,
) -> Result<CrawlSummary> {
    if req.rpc_urls.is_empty() {
        return Err(anyhow!("{}: no RPC endpoint configured", req.label));
    }
    let clients: Vec<RpcClient> = req
        .rpc_urls
        .iter()
        .map(|url| RpcClient::new_with_commitment(url.clone(), CommitmentConfig::confirmed()))
        .collect();
    let mut summary = CrawlSummary::default();
    let mut before = req.start_before;

    loop {
        if shutdown.is_cancelled() {
            return Err(anyhow!("{} crawl cancelled", req.label));
        }

        let page = try_endpoints(
            req.label,
            "getSignaturesForAddress",
            req.rpc_urls.len(),
            |i| fetch_page(&clients[i], &cfg.program_id, before, req.page_size + 1),
        )
        .await
        .with_context(|| format!("{} crawl: getSignaturesForAddress failed", req.label))?;
        let plan = plan_window(&page, req.stop_below, req.page_size);

        summary.windows += 1;
        summary.signatures_enumerated += plan.expected.len() as u64 + plan.failed.len() as u64;
        summary.signatures_expected += plan.expected.len() as u64;
        summary.signatures_failed += plan.failed.len() as u64;
        crate::metrics::add_backfill_signatures_fetched(page.len() as u64);
        if let Some(first) = plan.expected.first() {
            summary.newest_slot = summary.newest_slot.max(Some(first.slot));
        }
        if let Some(last) = plan.expected.last() {
            summary.oldest_slot = Some(match summary.oldest_slot {
                Some(existing) => existing.min(last.slot),
                None => last.slot,
            });
        }
        if !plan.failed.is_empty() {
            log::warn!(
                "{}: {} failed transaction(s) in this page are skipped, matching the live \
                 stream's `failed: false` filter: {}",
                req.label,
                plan.failed.len(),
                plan.failed
                    .iter()
                    .map(|s| s.signature.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        if plan.expected.is_empty() {
            log::info!(
                "{}: page {} has nothing to index (stop: {:?})",
                req.label,
                summary.windows,
                plan.stop
            );
        } else {
            log::info!(
                "{}: window {} covers {} transaction(s), slots {}..={}",
                req.label,
                summary.windows,
                plan.expected.len(),
                plan.expected.last().map(|s| s.slot).unwrap_or_default(),
                plan.expected.first().map(|s| s.slot).unwrap_or_default(),
            );

            let window = CrawlWindow {
                before,
                until: plan.until,
                page_size: req.page_size,
            };
            try_endpoints(req.label, "window delivery", req.rpc_urls.len(), |i| {
                run_window(
                    cfg,
                    &req.rpc_urls[i],
                    &deps,
                    window,
                    &plan.expected,
                    req.window_idle_timeout,
                    req.label,
                    shutdown.clone(),
                )
            })
            .await?;

            if req.report_progress {
                if let Some(oldest) = plan.expected.last() {
                    crate::metrics::set_backfill_last_processed_slot(oldest.slot);
                }
            }
        }

        // The cursor is pushed through the batcher rather than written directly, so it lands in
        // the same ordered stream as -- and never ahead of -- the rows of the page it describes.
        if req.persist_cursor {
            if let Some(cursor) = &plan.next_cursor {
                deps.batcher
                    .push(WriteOp::SetBackfillCursor {
                        signature: cursor.signature.to_string(),
                        slot: cursor.slot as i64,
                    })
                    .await
                    .map_err(|e| anyhow!("batcher channel closed: {e}"))?;
            }
        }

        if let Some(stop) = plan.stop {
            summary.stop = Some(stop);
            return Ok(summary);
        }

        before = plan.next_cursor.map(|c| c.signature);
        if before.is_none() {
            // Unreachable: a plan with no stop reason always carries a cursor. Treat it as a
            // hard error rather than looping forever from the tip.
            return Err(anyhow!(
                "{}: window {} produced neither a stop condition nor a cursor",
                req.label,
                summary.windows
            ));
        }
    }
}

/// Run `attempt` against endpoint 0, then 1, ... until one succeeds; return the last error if
/// none does.
///
/// Endpoints are addressed by index rather than by URL because the Alchemy JSON-RPC URL carries
/// the API key in its path. The per-attempt warning below still formats the underlying error
/// (useful for diagnosing throttling/outages), so that text is passed through
/// [`crate::config::redact_key`] before it reaches the log -- reqwest/solana-rpc-client's Error
/// Display can append the keyed URL via `" for url (<url>)"`, which would otherwise leak it.
async fn try_endpoints<T, F, Fut>(
    label: &str,
    what: &str,
    endpoints: usize,
    mut attempt: F,
) -> Result<T>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut last_err = None;
    for i in 0..endpoints {
        match attempt(i).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if i + 1 < endpoints {
                    log::warn!(
                        "{label}: {what} failed on RPC endpoint #{i} ({}); retrying on the \
                         next endpoint",
                        redact_key(&format!("{e:#}")),
                    );
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("{label}: no RPC endpoint configured for {what}")))
}

/// One `getSignaturesForAddress` page, newest first.
async fn fetch_page(
    rpc: &RpcClient,
    program_id: &Pubkey,
    before: Option<Signature>,
    limit: usize,
) -> Result<Vec<SigInfo>> {
    let page = rpc
        .get_signatures_for_address_with_config(
            program_id,
            GetConfirmedSignaturesForAddress2Config {
                before,
                until: None,
                limit: Some(limit),
                commitment: Some(CommitmentConfig::confirmed()),
            },
        )
        .await?;

    page.into_iter()
        .map(|s| {
            Ok(SigInfo {
                signature: Signature::from_str(&s.signature).with_context(|| {
                    format!("RPC returned an invalid signature: {}", s.signature)
                })?,
                slot: s.slot,
                failed: s.err.is_some(),
            })
        })
        .collect()
}

/// Run one crawler window and return once every expected signature has been processed.
#[allow(clippy::too_many_arguments)]
async fn run_window(
    cfg: &Config,
    rpc_url: &str,
    deps: &CrawlDeps<'_>,
    window: CrawlWindow,
    expected: &[SigInfo],
    idle_timeout: Duration,
    label: &str,
    shutdown: CancellationToken,
) -> Result<()> {
    let (obs_tx, mut obs_rx) = mpsc::unbounded_channel();
    // Cancelling this stops the crawler; the pipeline then drains and returns (see `Observed`).
    let window_token = CancellationToken::new();

    let mut pipe = pipeline::build_crawl(
        cfg,
        rpc_url,
        PipeDeps {
            batcher: deps.batcher,
            block_time: deps.block_time,
            tracked: deps.tracked,
            metrics: deps.metrics.clone(),
        },
        window,
        obs_tx,
        window_token.clone(),
    )
    .map_err(|e| anyhow!("building the {label} crawl pipeline failed: {e}"))?;

    let mut missing: HashSet<Signature> = expected.iter().map(|s| s.signature).collect();
    let total = missing.len();
    let watcher_token = window_token.clone();
    let watcher_shutdown = shutdown.clone();
    let label_owned = label.to_string();
    let watcher = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = watcher_shutdown.cancelled() => {
                    watcher_token.cancel();
                    return missing;
                }
                received = tokio::time::timeout(idle_timeout, obs_rx.recv()) => match received {
                    // Idle: the crawler has stopped delivering. Stop the window; the caller
                    // turns the still-missing set into a hard error.
                    Err(_) => {
                        log::error!(
                            "{label_owned}: no transaction delivered for {idle_timeout:?} with \
                             {}/{total} signature(s) still missing",
                            missing.len()
                        );
                        watcher_token.cancel();
                        return missing;
                    }
                    // Channel closed: the datasource is gone, nothing more will arrive.
                    Ok(None) => return missing,
                    Ok(Some(obs)) => {
                        missing.remove(&obs.signature);
                        if missing.is_empty() {
                            // Every expected transaction has been handed to the pipeline. Stop
                            // the crawler; `pipe.run()` returns once the queue is drained.
                            watcher_token.cancel();
                            return missing;
                        }
                    }
                },
            }
        }
    });

    let outcome = pipe.run().await;
    drop(pipe);
    window_token.cancel();
    // A panicked watcher must NOT read as "nothing missing" -- that would turn a bug into a
    // false completeness claim.
    let missing = watcher
        .await
        .map_err(|e| anyhow!("{label}: window watcher task failed: {e}"))?;

    if let Err(e) = outcome {
        return Err(anyhow!("{label} crawl pipeline failed: {e}"));
    }
    if !missing.is_empty() {
        return Err(anyhow!(
            "{label}: window incomplete -- {} of {} expected transaction(s) were never \
             delivered (first: {}). Nothing was marked complete; re-run to continue.",
            missing.len(),
            total,
            missing
                .iter()
                .next()
                .map(|s| s.to_string())
                .unwrap_or_default()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{plan_window, SigInfo, StopReason};
    use solana_signature::Signature;

    /// Deterministic dummy signatures: byte 0 = index, so `sig(3)` is stable and comparable.
    fn sig(n: u8) -> Signature {
        let mut bytes = [0u8; 64];
        bytes[0] = n;
        Signature::from(bytes)
    }

    fn page(entries: &[(u8, u64, bool)]) -> Vec<SigInfo> {
        entries
            .iter()
            .map(|&(n, slot, failed)| SigInfo {
                signature: sig(n),
                slot,
                failed,
            })
            .collect()
    }

    #[test]
    fn an_empty_page_means_the_history_is_exhausted() {
        let plan = plan_window(&[], 100, 10);
        assert_eq!(plan.stop, Some(StopReason::HistoryExhausted));
        assert!(plan.expected.is_empty());
        assert_eq!(plan.until, None);
        assert_eq!(plan.next_cursor, None);
    }

    #[test]
    fn a_short_page_ends_the_walk_and_indexes_all_of_it() {
        let plan = plan_window(&page(&[(1, 300, false), (2, 200, false)]), 100, 10);
        assert_eq!(plan.stop, Some(StopReason::HistoryExhausted));
        assert_eq!(plan.expected.len(), 2);
        // Nothing below the window, so the crawler needs no lower bound.
        assert_eq!(plan.until, None);
        // Nothing left to resume from either.
        assert_eq!(plan.next_cursor, None);
    }

    #[test]
    fn a_full_page_plus_the_probe_continues_below_the_window() {
        // page_size 3, so 4 entries: three in the window plus the probe.
        let plan = plan_window(
            &page(&[
                (1, 500, false),
                (2, 400, false),
                (3, 300, false),
                (4, 200, false),
            ]),
            100,
            3,
        );
        assert_eq!(plan.stop, None);
        assert_eq!(plan.expected.len(), 3);
        // The probe bounds the crawler's own enumeration...
        assert_eq!(plan.until, Some(sig(4)));
        // ...and the oldest signature IN the window is the resume cursor, so the probe is
        // re-enumerated (and indexed) by the next window rather than skipped.
        assert_eq!(plan.next_cursor.map(|c| c.signature), Some(sig(3)));
    }

    #[test]
    fn the_floor_truncates_the_window_and_stops_the_walk() {
        let plan = plan_window(
            &page(&[
                (1, 120, false),
                (2, 110, false),
                (3, 100, false), // exactly the floor: still indexed
                (4, 99, false),  // below the floor: not indexed, and ends the walk
                (5, 98, false),
            ]),
            100,
            10,
        );
        assert_eq!(plan.stop, Some(StopReason::ReachedFloor));
        assert_eq!(
            plan.expected.iter().map(|s| s.slot).collect::<Vec<_>>(),
            vec![120, 110, 100]
        );
        assert_eq!(plan.until, Some(sig(4)));
        assert_eq!(plan.next_cursor, None);
    }

    #[test]
    fn a_page_that_starts_below_the_floor_indexes_nothing() {
        let plan = plan_window(&page(&[(1, 50, false), (2, 40, false)]), 100, 10);
        assert_eq!(plan.stop, Some(StopReason::ReachedFloor));
        assert!(plan.expected.is_empty());
        assert_eq!(plan.until, Some(sig(1)));
    }

    #[test]
    fn failed_transactions_are_reported_but_never_expected() {
        let plan = plan_window(
            &page(&[(1, 300, false), (2, 250, true), (3, 200, false)]),
            100,
            10,
        );
        assert_eq!(
            plan.expected.iter().map(|s| s.slot).collect::<Vec<_>>(),
            vec![300, 200]
        );
        assert_eq!(
            plan.failed.iter().map(|s| s.slot).collect::<Vec<_>>(),
            vec![250]
        );
    }

    #[test]
    fn a_failed_signature_can_still_be_the_page_boundary() {
        // The cursor has to be the oldest signature of the window whether or not its
        // transaction succeeded -- `before` is about enumeration position, not about rows.
        let plan = plan_window(
            &page(&[
                (1, 500, false),
                (2, 400, false),
                (3, 300, true),
                (4, 200, false),
            ]),
            100,
            3,
        );
        assert_eq!(plan.next_cursor.map(|c| c.signature), Some(sig(3)));
        assert_eq!(plan.expected.len(), 2);
        assert_eq!(plan.failed.len(), 1);
    }
}
