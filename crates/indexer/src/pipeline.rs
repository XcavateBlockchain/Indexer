//! Datasource wiring: four decoders (one per program), two pipes each plus a shared deletion
//! pipe, one datasource.
//!
//! Two datasources are built here from the same processors:
//!
//! * [`build_live`] -- the Yellowstone gRPC stream (Alchemy devnet). Its job is **freshness**:
//!   sub-second visibility of new activity. One subscription carries per-program filter
//!   entries for every configured program.
//! * [`build_crawl`] -- one bounded window of the RPC transaction crawler
//!   (`getSignaturesForAddress`, newest -> oldest, between two signatures) for ONE program.
//!   Its job is **completeness**: it is what the history backfills and the periodic
//!   reconciliation supervisor are built from, and it is the only thing allowed to move a
//!   program's `sync_state.last_contiguous_slot` (see [`crate::sync_frontier`] for why the
//!   stream is not).
//!
//! Both feed *the same* processors and therefore the same batcher and the same tables. That is
//! the point: a crawl is evidence about the live path, not about a parallel code path.
//!
//! Every registered decoder self-filters -- it returns `None` for instructions/accounts that
//! are not its program's -- so all pairs safely share one datasource: an update is decoded by
//! exactly the one decoder that recognises it. Only CONFIGURED programs' pairs are
//! registered, though: datasource filters are per-TRANSACTION while decoding is
//! per-INSTRUCTION, so a transaction touching both a configured and an unconfigured program
//! passes the configured program's filter whole -- with the unconfigured program's pair
//! registered, its instructions in that transaction would be decoded and written as
//! fragmentary, never-backfilled history that the API would then serve as authoritative.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use carbon_core::error::CarbonResult;
use carbon_core::metrics::Metrics;
use carbon_core::pipeline::{Pipeline, ShutdownStrategy};
use carbon_marketplace_decoder::MarketplaceDecoder;
use carbon_property_decoder::PropertyDecoder;
use carbon_regions_decoder::RegionsDecoder;
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
    SubscribeRequestFilterAccounts, SubscribeRequestFilterTransactions,
};

use crate::batcher::Batcher;
use crate::block_time::BlockTimeResolver;
use crate::config::Config;
use crate::crawl::{CrawlWindow, ObservationSender, Observed};
use crate::mapping::{
    marketplace::Marketplace, property::Property, regions::Regions, whitelist::Whitelist,
};
use crate::processors::{
    AccountDeletionProcessor, AccountProcessor, InstructionProcessor, TrackedAccounts,
};
use crate::programs::ProgramSpec;

/// The narrowest transaction filters the programs allow: one keyed entry per program, each
/// matching that program only, no votes, no failures. Yellowstone echoes the keys back on
/// each update; they are arbitrary but show up in server-side logs, so they are named after
/// what they select (`<program>_txs`).
///
/// One entry PER PROGRAM is load-bearing: `account_required` is an AND over its list, so a
/// single entry listing all four programs would match only transactions touching ALL of them
/// -- i.e. almost nothing. (`account_include` is the OR form, but it can accidentally widen
/// if another account is ever added to a list; per-program AND entries cannot.)
///
/// `failed: Some(false)` matches the old SubQuery handlers, which never saw failed
/// transactions and therefore never recorded them. Indexing them now would add rows the old
/// database does not have and break parity.
pub fn transaction_filters(
    programs: &[&'static ProgramSpec],
) -> HashMap<String, SubscribeRequestFilterTransactions> {
    programs
        .iter()
        .map(|p| {
            (
                format!("{}_txs", p.name),
                SubscribeRequestFilterTransactions {
                    vote: Some(false),
                    failed: Some(false),
                    signature: None,
                    account_include: vec![],
                    account_exclude: vec![],
                    account_required: vec![p.id.to_string()],
                },
            )
        })
        .collect()
}

/// Owner-scoped account filters: one keyed entry per program, each selecting every account
/// owned by that program -- exactly the PDA types its decoder knows. No `filters`
/// (memcmp/datasize) -- the decoders discriminate by discriminator anyway, and a server-side
/// filter would silently drop any account type added to a program in future.
pub fn account_filters(
    programs: &[&'static ProgramSpec],
) -> HashMap<String, SubscribeRequestFilterAccounts> {
    programs
        .iter()
        .map(|p| {
            (
                format!("{}_accounts", p.name),
                SubscribeRequestFilterAccounts {
                    account: vec![],
                    owner: vec![p.id.to_string()],
                    filters: vec![],
                    nonempty_txn_signature: None,
                },
            )
        })
        .collect()
}

/// Everything both pipelines need that is not datasource-specific.
///
/// `metrics` is a field rather than always `PrometheusMetrics` so a caller can wrap or replace
/// the recorder (tests pass a no-op; a future job could count updates through it).
pub struct PipeDeps<'a> {
    pub batcher: &'a Batcher,
    pub block_time: &'a Arc<BlockTimeResolver>,
    pub tracked: &'a TrackedAccounts,
    pub metrics: Arc<dyn Metrics>,
    /// The configured program set (`Config::programs`): only these programs' decoder+
    /// processor pairs are registered (see the module docs on why registration must track
    /// the datasource filters).
    pub programs: &'a [&'static ProgramSpec],
}

/// Shared builder: the pipes are identical for both datasources, only the source differs.
///
/// One decoder+processor pair per CONFIGURED program (see the module docs): the pair set
/// tracks the datasource filter set exactly, so a transaction shared between a configured
/// and an unconfigured program contributes only the configured program's instructions. The
/// deletion pipe is shared -- pubkeys are globally unique, and only tracked (i.e.
/// configured) accounts ever produce a deletion event.
fn common_pipes(deps: PipeDeps<'_>) -> carbon_core::pipeline::PipelineBuilder {
    let configured = |name: &str| deps.programs.iter().any(|p| p.name == name);
    let mut builder = Pipeline::builder()
        .metrics(deps.metrics)
        .metrics_flush_interval(5)
        .shutdown_strategy(ShutdownStrategy::ProcessPending);
    if configured("xcavate_whitelist") {
        builder = builder
            .instruction(
                XcavateWhitelistDecoder,
                InstructionProcessor::<Whitelist>::new(
                    deps.batcher.clone(),
                    deps.block_time.clone(),
                ),
            )
            .account(
                XcavateWhitelistDecoder,
                AccountProcessor::<Whitelist>::new(deps.batcher.clone(), deps.tracked.clone()),
            );
    }
    if configured("regions") {
        builder = builder
            .instruction(
                RegionsDecoder,
                InstructionProcessor::<Regions>::new(deps.batcher.clone(), deps.block_time.clone()),
            )
            .account(
                RegionsDecoder,
                AccountProcessor::<Regions>::new(deps.batcher.clone(), deps.tracked.clone()),
            );
    }
    if configured("marketplace") {
        builder = builder
            .instruction(
                MarketplaceDecoder,
                InstructionProcessor::<Marketplace>::new(
                    deps.batcher.clone(),
                    deps.block_time.clone(),
                ),
            )
            .account(
                MarketplaceDecoder,
                AccountProcessor::<Marketplace>::new(deps.batcher.clone(), deps.tracked.clone()),
            );
    }
    if configured("property") {
        builder = builder
            .instruction(
                PropertyDecoder,
                InstructionProcessor::<Property>::new(
                    deps.batcher.clone(),
                    deps.block_time.clone(),
                ),
            )
            .account(
                PropertyDecoder,
                AccountProcessor::<Property>::new(deps.batcher.clone(), deps.tracked.clone()),
            );
    }
    builder.account_deletions(AccountDeletionProcessor::new(deps.batcher.clone()))
}

/// The live Yellowstone pipeline, subscribed to every configured program. `tracked` must
/// already be seeded from the database (see `db::close::open_account_pubkeys`) -- the
/// datasource only emits deletions for pubkeys in that set.
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
        Some(yellowstone_grpc_proto::geyser::CommitmentLevel::Confirmed),
        account_filters(&cfg.programs),
        transaction_filters(&cfg.programs),
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

/// One bounded window of the RPC transaction crawler: every signature that touched
/// `program_id` between `window.before` (exclusive, newer end) and `window.until` (exclusive,
/// older end), newest -> oldest, pushed through the same pipes as the live stream.
///
/// `observer` receives every transaction update *before* it reaches the pipeline, which is how
/// the caller knows when the window has been delivered in full (see [`crate::crawl`]).
///
/// The crawler is transaction-only (`update_types()` is `[Transaction]`), so the account and
/// deletion pipes never fire on this path; account state comes from the live stream and from
/// the `getProgramAccounts` snapshots instead.
pub fn build_crawl(
    program_id: Pubkey,
    rpc_url: &str,
    deps: PipeDeps<'_>,
    window: CrawlWindow,
    observer: ObservationSender,
    cancellation: CancellationToken,
) -> CarbonResult<Pipeline> {
    let crawler = RpcTransactionCrawler::new(
        rpc_url.to_string(),
        program_id,
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
            // aggressively (see MIGRATION_LOG.md) and each program's whole history is only a
            // few hundred transactions, so there is nothing to gain from hammering it.
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
