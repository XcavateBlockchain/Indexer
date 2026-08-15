//! Datasource wiring: one decoder, two pipes plus a deletion pipe, one datasource.
//!
//! Two datasources are built here from the same processors:
//!
//! * [`build_live`] -- the Yellowstone gRPC stream (Alchemy devnet). Its job is **freshness**:
//!   sub-second visibility of new activity.
//! * [`build_crawl`] -- one bounded window of the RPC transaction crawler
//!   (`getSignaturesForAddress`, newest -> oldest, between two signatures). Its job is
//!   **completeness**: it is what the history backfill and the periodic reconciliation
//!   supervisor are built from, and it is the only thing allowed to move
//!   `sync_state.last_contiguous_slot` (see [`crate::sync_frontier`] for why the stream is not).
//!
//! Both feed *the same* processors and therefore the same batcher and the same tables. That is
//! the point: a crawl is evidence about the live path, not about a parallel code path.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use carbon_core::error::CarbonResult;
use carbon_core::metrics::Metrics;
use carbon_core::pipeline::{Pipeline, ShutdownStrategy};
use carbon_rpc_transaction_crawler_datasource::{
    ConnectionConfig, Filters, RetryConfig, RpcTransactionCrawler,
};
use carbon_xcavate_whitelist_decoder::XcavateWhitelistDecoder;
use carbon_yellowstone_grpc_datasource::{
    BlockFilters, YellowstoneGrpcClientConfig, YellowstoneGrpcGeyserClient,
};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use yellowstone_grpc_proto::geyser::{
    CommitmentLevel, SubscribeRequestFilterAccounts, SubscribeRequestFilterTransactions,
};

use crate::batcher::Batcher;
use crate::block_time::BlockTimeResolver;
use crate::config::Config;
use crate::crawl::{CrawlWindow, ObservationSender, Observed};
use crate::processors::{
    AccountDeletionProcessor, AccountProcessor, InstructionProcessor, TrackedAccounts,
};

/// Filter-map keys. Yellowstone echoes these back on each update; they are arbitrary but show
/// up in server-side logs, so they are named after what they select.
const TX_FILTER_KEY: &str = "xcavate_whitelist_txs";
const ACCOUNT_FILTER_KEY: &str = "xcavate_whitelist_accounts";

/// The narrowest transaction filter the program allows: this program only, no votes, no
/// failures.
///
/// `account_required` (not `account_include`) is deliberate -- `account_include` is an OR over
/// the listed accounts, `account_required` an AND, and with a single entry the AND form is the
/// one that cannot accidentally widen if another account is ever added to the list.
///
/// `failed: Some(false)` matches the old SubQuery handlers, which never saw failed
/// transactions and therefore never recorded them. Indexing them now would add rows the old
/// database does not have and break parity.
pub fn transaction_filters(
    program_id: &Pubkey,
) -> HashMap<String, SubscribeRequestFilterTransactions> {
    HashMap::from([(
        TX_FILTER_KEY.to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            signature: None,
            account_include: vec![],
            account_exclude: vec![],
            account_required: vec![program_id.to_string()],
        },
    )])
}

/// Owner-scoped account filter: every account owned by the program, which is exactly the three
/// PDA types the decoder knows. No `filters` (memcmp/datasize) -- the decoder discriminates by
/// discriminator anyway, and a server-side filter would silently drop any account type added
/// to the program in future.
pub fn account_filters(program_id: &Pubkey) -> HashMap<String, SubscribeRequestFilterAccounts> {
    HashMap::from([(
        ACCOUNT_FILTER_KEY.to_string(),
        SubscribeRequestFilterAccounts {
            account: vec![],
            owner: vec![program_id.to_string()],
            filters: vec![],
            nonempty_txn_signature: None,
        },
    )])
}

/// Everything both pipelines need that is not datasource-specific.
///
/// `metrics` is a field rather than always `PrometheusMetrics` so the `replay` subcommand can
/// wrap it and observe update arrivals (it needs them to know when the crawl has run out of
/// history).
pub struct PipeDeps<'a> {
    pub batcher: &'a Batcher,
    pub block_time: &'a Arc<BlockTimeResolver>,
    pub tracked: &'a TrackedAccounts,
    pub metrics: Arc<dyn Metrics>,
}

/// Shared builder: the pipes are identical for both datasources, only the source differs.
fn common_pipes(deps: PipeDeps<'_>) -> carbon_core::pipeline::PipelineBuilder {
    Pipeline::builder()
        .metrics(deps.metrics)
        .metrics_flush_interval(5)
        .shutdown_strategy(ShutdownStrategy::ProcessPending)
        .instruction(
            XcavateWhitelistDecoder,
            InstructionProcessor::new(deps.batcher.clone(), deps.block_time.clone()),
        )
        .account(
            XcavateWhitelistDecoder,
            AccountProcessor::new(deps.batcher.clone(), deps.tracked.clone()),
        )
        .account_deletions(AccountDeletionProcessor::new(deps.batcher.clone()))
}

/// The live Yellowstone pipeline. `tracked` must already be seeded from the database (see
/// `db::accounts::open_account_pubkeys`) -- the datasource only emits deletions for pubkeys in
/// that set.
pub fn build_live(
    cfg: &Config,
    api_key: &str,
    deps: PipeDeps<'_>,
    cancellation: CancellationToken,
) -> CarbonResult<Pipeline> {
    let datasource = YellowstoneGrpcGeyserClient::new(
        cfg.grpc_url.clone(),
        // Alchemy authenticates the gRPC endpoint with the key in the X-Token header, which is
        // what the datasource does with this argument. It is NOT part of the URL (unlike the
        // JSON-RPC endpoint), so the endpoint string is safe to log.
        Some(api_key.to_string()),
        Some(CommitmentLevel::Confirmed),
        account_filters(&cfg.program_id),
        transaction_filters(&cfg.program_id),
        // No block subscription: it would deliver every transaction in every block on devnet
        // just to get block metadata we can obtain far more cheaply from `getBlockTime`.
        BlockFilters::default(),
        deps.tracked.clone(),
        YellowstoneGrpcClientConfig::default(),
    );

    common_pipes(deps)
        .datasource(datasource)
        .datasource_cancellation_token(cancellation)
        .build()
}

/// One bounded window of the RPC transaction crawler: every signature that touched the program
/// between `window.before` (exclusive, newer end) and `window.until` (exclusive, older end),
/// newest -> oldest, pushed through the same pipes as the live stream.
///
/// `observer` receives every transaction update *before* it reaches the pipeline, which is how
/// the caller knows when the window has been delivered in full (see [`crate::crawl`]).
///
/// The crawler is transaction-only (`update_types()` is `[Transaction]`), so the account and
/// deletion pipes never fire on this path; account state comes from the live stream and from
/// the `getProgramAccounts` snapshot instead.
pub fn build_crawl(
    cfg: &Config,
    rpc_url: &str,
    deps: PipeDeps<'_>,
    window: CrawlWindow,
    observer: ObservationSender,
    cancellation: CancellationToken,
) -> CarbonResult<Pipeline> {
    let crawler = RpcTransactionCrawler::new(
        rpc_url.to_string(),
        cfg.program_id,
        ConnectionConfig::new(
            // `getSignaturesForAddress` page size, matched to the caller's own page size so the
            // crawler enumerates exactly the window the caller planned.
            window.page_size,
            // How long the crawler waits after exhausting the window before polling again. We
            // cancel long before that matters -- a window is finished the moment its last
            // expected signature has been delivered -- so this only bounds how long a
            // fully-delivered window's crawler tasks idle before teardown.
            Duration::from_secs(2),
            // Concurrent `getTransaction` calls. Kept low: Alchemy's free tier throttles
            // aggressively (see MIGRATION_LOG.md) and the whole history is only a few hundred
            // transactions, so there is nothing to gain from hammering it.
            3,
            RetryConfig::default(),
            None,
            None,
            // MUST be true. With `blocking_send` false the crawler uses `try_send` and drops
            // updates whenever the pipeline channel is momentarily full -- silently losing
            // history, which is precisely what a completeness crawl exists to prevent.
            true,
        ),
        Filters::new(None, window.before, window.until),
        Some(CommitmentConfig::confirmed()),
    );

    common_pipes(deps)
        .datasource(Observed::new(crawler, observer, cancellation))
        // Deliberately NOT `.datasource_cancellation_token(...)`: carbon's `run()` loop breaks
        // *immediately* when that token fires (ShutdownStrategy::ProcessPending only covers its
        // own SIGINT branch), which would drop updates still queued in the pipeline channel.
        // `Observed` owns the cancellation instead: cancelling it stops the crawler, which
        // closes the channel, which makes `run()` drain everything pending and only then
        // return. That is what lets the caller read "run() returned" as "every delivered
        // transaction has been mapped and pushed to the batcher".
        .build()
}

/// A fresh, empty tracked-account set.
pub fn new_tracked_accounts() -> TrackedAccounts {
    Arc::new(RwLock::new(HashSet::new()))
}
