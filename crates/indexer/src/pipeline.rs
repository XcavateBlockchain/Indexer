//! Datasource wiring: one decoder, two pipes plus a deletion pipe, one datasource.
//!
//! Two datasources are built here from the same processors:
//!
//! * [`build_live`] -- the Yellowstone gRPC stream (Alchemy devnet), the production path.
//! * [`build_replay`] -- the RPC transaction crawler over the program's whole signature
//!   history, used by the `replay` subcommand to verify the pipeline against real chain data
//!   (exit check, ruling R3: this program has no signing keys available, so a synthetic
//!   end-to-end test is impossible and real history is the substitute).
//!
//! Both feed *the same* processors and therefore the same batcher and the same tables. That is
//! the point: the replay is evidence about the live path, not about a parallel code path.

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
use crate::processors::{
    AccountDeletionProcessor, AccountProcessor, InstructionProcessor, SessionMarker,
    TrackedAccounts,
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
fn common_pipes(
    deps: PipeDeps<'_>,
    session: Option<Arc<SessionMarker>>,
) -> carbon_core::pipeline::PipelineBuilder {
    Pipeline::builder()
        .metrics(deps.metrics)
        .metrics_flush_interval(5)
        .shutdown_strategy(ShutdownStrategy::ProcessPending)
        .instruction(
            XcavateWhitelistDecoder,
            InstructionProcessor::new(
                deps.batcher.clone(),
                deps.block_time.clone(),
                session.clone(),
            ),
        )
        .account(
            XcavateWhitelistDecoder,
            AccountProcessor::new(deps.batcher.clone(), deps.tracked.clone(), session),
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
    session: Arc<SessionMarker>,
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

    common_pipes(deps, Some(session))
        .datasource(datasource)
        .datasource_cancellation_token(cancellation)
        .build()
}

/// The historical replay pipeline: crawl every signature that ever touched the program and
/// push each transaction through the same pipes.
///
/// Note the crawler is transaction-only (`update_types()` is `[Transaction]`), so the account
/// and deletion pipes simply never fire on this path; account state during a replay comes from
/// nothing at all, which is why the replay's job is verifying `program_instructions` /
/// `whitelist_actions` and the gRPC smoke check covers account streaming.
pub fn build_replay(
    cfg: &Config,
    rpc_url: &str,
    deps: PipeDeps<'_>,
    cancellation: CancellationToken,
) -> CarbonResult<Pipeline> {
    let datasource = RpcTransactionCrawler::new(
        rpc_url.to_string(),
        cfg.program_id,
        ConnectionConfig::new(
            // `getSignaturesForAddress` page size; 100 is the crawler's own default and well
            // inside every provider's limit.
            100,
            // How long to wait after exhausting history before polling for new signatures.
            // Short, because the replay's idle-timeout watchdog uses these empty polls as its
            // "history is done" signal.
            Duration::from_secs(2),
            // Concurrent `getTransaction` calls. Kept low: Alchemy's free tier throttles
            // aggressively (see MIGRATION_LOG.md) and the whole history is only a few hundred
            // transactions, so there is nothing to gain from hammering it.
            3,
            RetryConfig::default(),
            None,
            None,
            // MUST be true. With `blocking_send` false the crawler uses `try_send` and drops
            // updates whenever the pipeline's channel is momentarily full -- silently losing
            // history, which is precisely what this replay is meant to prove does not happen.
            true,
        ),
        Filters::new(None, None, None),
        Some(CommitmentConfig::confirmed()),
    );

    common_pipes(deps, None)
        .datasource(datasource)
        .datasource_cancellation_token(cancellation)
        .build()
}

/// A fresh, empty tracked-account set.
pub fn new_tracked_accounts() -> TrackedAccounts {
    Arc::new(RwLock::new(HashSet::new()))
}
