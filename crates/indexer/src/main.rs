//! The indexer binary.
//!
//! ```text
//! indexer run          # live gRPC stream + snapshot/backfill on startup + reconciliation
//! indexer snapshot     # one-shot getProgramAccounts state snapshot
//! indexer backfill     # resumable history walk down to the floor slot
//! indexer smoke-grpc   # connectivity/auth/filter check against the Yellowstone endpoint
//! ```
//!
//! `snapshot` and `backfill` are subcommands precisely so they can be run by hand against a
//! production `DATABASE_URL` without redeploying; both are idempotent, so re-running one is
//! always safe.
//!
//! Configuration is entirely environmental -- see `indexer::config`. Load the repo `.env` into
//! the shell first (`set -a; . ./.env; set +a`): the binary deliberately does not read a dotenv
//! file itself, so the process sees exactly what the operator (or the container runtime)
//! exported, with no third source of truth to reconcile.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use indexer::backfill::{self, BackfillOptions};
use indexer::batcher;
use indexer::block_time::BlockTimeResolver;
use indexer::config::{redact_url_password, Config};
use indexer::db;
use indexer::metrics::PrometheusMetrics;
use indexer::pipeline::{self, PipeDeps};
use indexer::sync_frontier::SyncFrontier;
use indexer::{reconcile, snapshot};
use solana_pubkey::Pubkey;
use tokio_util::sync::CancellationToken;

/// gRPC reconnect backoff bounds.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A stream session that lasted at least this long counts as healthy, and resets the backoff.
const HEALTHY_SESSION: Duration = Duration::from_secs(120);
/// How long the startup subscribe gate waits for its first update. The slot heartbeat arrives
/// every ~400 ms, so this only has to cover connection setup on a slow link.
const SUBSCRIBE_GATE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `run` lets the live subscription settle before starting the snapshot.
///
/// carbon's datasource subscribes inside a spawned task and exposes no "subscribed" signal, so
/// there is nothing to await. This is not the correctness mechanism -- the slot guard is (see
/// `indexer::snapshot`) -- it just makes the overlap between stream and snapshot the norm
/// rather than a race.
const STREAM_SETTLE: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(
    name = "indexer",
    about = "Xcavate whitelist indexer (Carbon pipeline)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the live Yellowstone gRPC pipeline, plus the startup snapshot/backfill and the
    /// reconciliation supervisor.
    Run,
    /// Connect to the Yellowstone endpoint, subscribe with the production filters plus a slot
    /// heartbeat, and report the first update.
    SmokeGrpc {
        /// Seconds to wait for the first update.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Walk the program's transaction history newest -> oldest down to the floor slot, through
    /// the same pipes as the live stream. Resumable; safe to re-run.
    Backfill {
        /// Stop below this slot. Defaults to `sync_state.backfill_floor_slot`.
        #[arg(long)]
        floor: Option<u64>,
        /// `getSignaturesForAddress` page size (also the commit/cursor granularity).
        #[arg(long, default_value_t = backfill::DEFAULT_PAGE_SIZE)]
        page_size: usize,
        /// Seconds a window may go without a delivery before the walk is declared stuck.
        #[arg(long, default_value_t = backfill::DEFAULT_WINDOW_IDLE_TIMEOUT.as_secs())]
        window_idle_timeout: u64,
        /// Serve `/metrics` during the walk. Off by default so a manual backfill can run
        /// alongside a live indexer without fighting over the port.
        #[arg(long)]
        metrics: bool,
    },
    /// Take a `getProgramAccounts` snapshot of current account state and write it through the
    /// slot-guarded upserts.
    Snapshot {
        /// Serve `/metrics` during the snapshot (see `backfill --metrics`).
        #[arg(long)]
        metrics: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // `RUST_LOG` controls verbosity; default to info for our crate and warn for the noisy
    // transport crates underneath.
    env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("info,hyper=warn,h2=warn,tonic=warn,rustls=warn"),
    )
    .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Run => run_live().await,
        Command::SmokeGrpc { timeout } => run_smoke(Duration::from_secs(timeout)).await,
        Command::Backfill {
            floor,
            page_size,
            window_idle_timeout,
            metrics,
        } => {
            run_backfill(
                BackfillOptions {
                    floor,
                    page_size,
                    window_idle_timeout: Duration::from_secs(window_idle_timeout),
                },
                metrics,
            )
            .await
        }
        Command::Snapshot { metrics } => run_snapshot(metrics).await,
    }
}

/// Shared startup: migrations, sync-state seeding, and the pieces every subcommand needs.
struct Started {
    cfg: Config,
    pool: sqlx::PgPool,
    frontier: Arc<SyncFrontier>,
    block_time: Arc<BlockTimeResolver>,
    tracked: indexer::processors::TrackedAccounts,
    state: db::sync_state::SyncState,
}

async fn start() -> Result<Started> {
    let cfg = Config::from_env()?;
    let database_url = cfg.require_database_url()?.to_string();
    log::info!(
        "starting indexer: program={} database={} grpc={} backfill_start_slot={} metrics={}",
        cfg.program_id,
        redact_url_password(&database_url),
        cfg.grpc_url,
        cfg.backfill_start_slot,
        cfg.metrics_addr,
    );

    let pool = db::connect(&database_url)
        .await
        .with_context(|| format!("connecting to {}", redact_url_password(&database_url)))?;

    db::run_migrations(&pool)
        .await
        .context("running migrations")?;
    log::info!("migrations applied");

    db::sync_state::init_sync_state(&pool, cfg.backfill_start_slot as i64)
        .await
        .context("seeding sync_state")?;
    let state = db::sync_state::get_sync_state(&pool)
        .await
        .context("reading sync_state")?
        .context("sync_state row missing immediately after init")?;
    log::info!(
        "sync_state: last_contiguous_slot={} backfill_complete={} backfill_floor_slot={} snapshot_slot={:?}",
        state.last_contiguous_slot,
        state.backfill_complete,
        state.backfill_floor_slot,
        state.snapshot_slot,
    );

    let frontier = Arc::new(SyncFrontier::new(state.backfill_complete));

    // Seed the deletion-tracking set from the database. Without this, a restarted process
    // would be blind to the closure of every PDA it had not yet seen an update for.
    let tracked = pipeline::new_tracked_accounts();
    let seeds = db::accounts::open_account_pubkeys(&pool)
        .await
        .context("seeding account_deletions_tracked")?;
    {
        let mut set = tracked.write().await;
        for bytes in &seeds {
            match Pubkey::try_from(bytes.as_slice()) {
                Ok(pk) => {
                    set.insert(pk);
                }
                Err(_) => log::error!(
                    "account-state row has a {}-byte pubkey; skipping it in the deletion tracker",
                    bytes.len()
                ),
            }
        }
        log::info!(
            "seeded {} tracked account(s) for deletion watching",
            set.len()
        );
    }

    let block_time = Arc::new(BlockTimeResolver::new(
        &cfg.rpc_url(),
        &cfg.rpc_fallback_url,
    ));

    Ok(Started {
        cfg,
        pool,
        frontier,
        block_time,
        tracked,
        state,
    })
}

async fn run_live() -> Result<()> {
    let started = start().await?;
    let cfg = &started.cfg;
    let api_key = cfg.require_api_key()?.to_string();

    indexer::metrics::install(cfg.metrics_addr)?;
    indexer::metrics::set_last_contiguous_slot(started.state.last_contiguous_slot.max(0) as u64);

    // --- startup subscribe gate --------------------------------------------------------------
    // carbon's Yellowstone datasource subscribes inside a spawned task and only `log::error!`s a
    // plan/auth rejection, then retries forever: a bad key would leave this process looking
    // alive while indexing nothing. So assert the subscription once, up front, with the exact
    // production filters, and refuse to start if it is rejected.
    let gate = indexer::grpc_smoke::run(cfg, SUBSCRIBE_GATE_TIMEOUT)
        .await
        .context("startup subscribe gate failed -- refusing to start with an unusable stream")?;
    let connected_slot = gate.slot;
    log::info!(
        "subscribe gate passed: {} accepted the production filters (first update: {gate})",
        cfg.grpc_url
    );

    let shutdown = CancellationToken::new();
    spawn_ctrl_c_watcher(shutdown.clone());

    let (batcher, flusher) = batcher::spawn(started.pool.clone(), shutdown.clone());

    // --- ORDERING (spec §7, non-negotiable) --------------------------------------------------
    // The live stream starts FIRST and the snapshot/backfill run behind it as background tasks.
    //
    // Why: `getProgramAccounts` takes a while, and any account that changes between the
    // snapshot's read and the stream's subscription is invisible to both -- the snapshot has the
    // pre-change value, the stream never saw the change. That is a permanent hole exactly as
    // wide as the snapshot. Subscribing first makes the two overlap instead, and the slot guard
    // resolves the overlap: anything the stream already wrote at a higher slot survives the
    // snapshot's upsert untouched. This looks like removable complexity; it is not.
    let jobs = spawn_startup_jobs(&started, connected_slot, shutdown.clone());

    // The reconciliation supervisor: the only writer of `last_contiguous_slot` (see
    // `indexer::reconcile` for why the live stream cannot be trusted with it).
    let supervisor = {
        let cfg = Config::from_env()?;
        let pool = started.pool.clone();
        let frontier = started.frontier.clone();
        let block_time = started.block_time.clone();
        let tracked = started.tracked.clone();
        let shutdown = shutdown.clone();
        let interval = cfg.reconcile_interval;
        tokio::spawn(async move {
            reconcile::supervise(
                &cfg,
                &pool,
                &frontier,
                &block_time,
                &tracked,
                interval,
                shutdown,
            )
            .await
        })
    };

    let mut backoff = RECONNECT_BACKOFF_MIN;
    while !shutdown.is_cancelled() {
        // Child token so cancelling the process cancels the datasource, but a datasource
        // teardown does not cancel the process.
        let ds_cancel = shutdown.child_token();

        let mut pipeline = pipeline::build_live(
            cfg,
            &api_key,
            PipeDeps {
                batcher: &batcher,
                block_time: &started.block_time,
                tracked: &started.tracked,
                metrics: Arc::new(PrometheusMetrics),
            },
            ds_cancel.clone(),
        )
        .map_err(|e| anyhow::anyhow!("building the live pipeline failed: {e}"))?;

        log::info!("pipeline built; subscribing to {}", cfg.grpc_url);
        let session_start = std::time::Instant::now();
        let outcome = pipeline.run().await;
        let session_duration = session_start.elapsed();
        drop(pipeline);
        ds_cancel.cancel();

        if shutdown.is_cancelled() {
            match outcome {
                Ok(()) => log::info!("pipeline stopped for shutdown"),
                Err(e) => log::error!("pipeline stopped for shutdown with an error: {e}"),
            }
            break;
        }

        // The stream session ended without us asking it to: whatever happened on chain in the
        // meantime may be a hole. The next reconciliation crawl re-covers that range and closes
        // the gap; until then the frontier is frozen.
        started.frontier.gap_opened();
        indexer::metrics::inc_grpc_reconnect();

        // A session that stayed up for a while was healthy; the next failure is a new
        // incident, not a continuation of the last one, so the backoff starts over. Without
        // this reset a single bad hour would leave the process reconnecting at the 60 s
        // ceiling for the rest of its life.
        if session_duration >= HEALTHY_SESSION {
            backoff = RECONNECT_BACKOFF_MIN;
        }

        match outcome {
            Ok(()) => log::warn!(
                "gRPC stream ended cleanly after {session_duration:?}; reconnecting in {backoff:?}"
            ),
            Err(e) => log::error!(
                "gRPC stream failed after {session_duration:?} ({e}); reconnecting in {backoff:?}"
            ),
        }

        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.cancelled() => break,
        }
        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
    }

    shutdown.cancel();
    supervisor.await.ok();
    jobs.await.ok();

    // Dropping the last Batcher closes the channel, which makes the flusher commit its final
    // partial batch and exit.
    drop(batcher);
    // FINDING 2 (Task-4 fix round): this batcher writes no completion marker itself (that moved
    // to the one-shot jobs and the reconciler), so there is nothing here to skip -- but a
    // laundered drop on the way out would still be a silent data loss an operator should know
    // about, so report it instead of swallowing it with `.ok()`.
    let flush_outcome = flusher.await.unwrap_or(batcher::FlushOutcome::OpsDropped);
    if flush_outcome.all_committed() {
        log::info!("shutdown complete");
    } else {
        log::warn!(
            "shutdown complete, but the final batch(es) from the live pipeline were dropped \
             uncommitted (a double fault: a DB commit kept failing and shutdown fired during \
             its retry backoff, see indexer::batcher::flush). No completion marker here depends \
             on those rows, so this is not a correctness problem by itself; the dropped \
             instructions/accounts are simply not indexed yet and will be re-derived on the \
             next start by the live stream (if still recent) or by `indexer backfill` \
             (idempotent re-walk covers any gap)."
        );
    }
    Ok(())
}

/// The two one-shot startup jobs, in the order spec §7 requires: snapshot (if the database has
/// never had one) then history backfill (if it never completed). Both run behind the live
/// stream and neither blocks it; each owns its own batcher, so a slow backfill cannot stall the
/// live pipe's writes.
fn spawn_startup_jobs(
    started: &Started,
    connected_slot: Option<u64>,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let need_snapshot = started.state.snapshot_slot.is_none();
    let need_backfill = !started.state.backfill_complete;
    let pool = started.pool.clone();
    let frontier = started.frontier.clone();
    let block_time = started.block_time.clone();
    let tracked = started.tracked.clone();

    tokio::spawn(async move {
        let Ok(cfg) = Config::from_env() else { return };

        if !need_snapshot && !need_backfill {
            log::info!(
                "startup jobs: nothing to do (snapshot_slot is set and backfill_complete is true)"
            );
            return;
        }

        // See STREAM_SETTLE: the stream has to be subscribed before the snapshot reads state.
        tokio::select! {
            _ = tokio::time::sleep(STREAM_SETTLE) => {}
            _ = shutdown.cancelled() => return,
        }

        if need_snapshot {
            log::info!(
                "STARTUP JOB 1/2: taking the getProgramAccounts snapshot (stream connected at \
                 slot {connected_slot:?}; the stream has been writing since before this read, so \
                 no account change can fall between them)"
            );
            match snapshot::run(&cfg, &pool, &tracked, shutdown.clone()).await {
                Ok(s) => log::info!(
                    "STARTUP JOB 1/2 done: {} account(s) at slot {}{}",
                    s.accounts_loaded,
                    s.slot,
                    if s.undecodable > 0 {
                        format!(" ({} undecodable!)", s.undecodable)
                    } else {
                        String::new()
                    }
                ),
                Err(_) if shutdown.is_cancelled() => {
                    log::info!("STARTUP JOB 1/2 stopped for shutdown; re-runs on next start");
                    return;
                }
                Err(e) => log::error!(
                    "STARTUP JOB 1/2 FAILED: snapshot did not complete ({e:#}); account-state \
                     tables may be empty until `indexer snapshot` is run by hand"
                ),
            }
        } else {
            log::info!("STARTUP JOB 1/2 skipped: sync_state.snapshot_slot is already set");
        }

        if shutdown.is_cancelled() {
            return;
        }

        if need_backfill {
            log::info!("STARTUP JOB 2/2: running the history backfill down to the floor slot");
            match backfill::run(
                &cfg,
                &pool,
                &frontier,
                &block_time,
                &tracked,
                BackfillOptions::default(),
                shutdown.clone(),
            )
            .await
            {
                Ok(s) => log::info!(
                    "STARTUP JOB 2/2 done: {} signature(s) indexed across {} window(s)",
                    s.signatures_expected,
                    s.windows
                ),
                Err(_) if shutdown.is_cancelled() => log::info!(
                    "STARTUP JOB 2/2 stopped for shutdown; it resumes from its cursor on the \
                     next start"
                ),
                Err(e) => log::error!(
                    "STARTUP JOB 2/2 FAILED: history backfill did not reach the floor ({e:#}); \
                     backfill_complete stays false and last_contiguous_slot stays frozen. Re-run \
                     `indexer backfill` -- it resumes from its cursor."
                ),
            }
        } else {
            log::info!("STARTUP JOB 2/2 skipped: sync_state.backfill_complete is already true");
        }
    })
}

async fn run_backfill(opts: BackfillOptions, serve_metrics: bool) -> Result<()> {
    let started = start().await?;
    if serve_metrics {
        if let Err(e) = indexer::metrics::install(started.cfg.metrics_addr) {
            log::warn!("continuing without a metrics listener: {e}");
        }
    }

    let shutdown = CancellationToken::new();
    spawn_ctrl_c_watcher(shutdown.clone());

    let summary = backfill::run(
        &started.cfg,
        &started.pool,
        &started.frontier,
        &started.block_time,
        &started.tracked,
        opts,
        shutdown,
    )
    .await?;

    println!(
        "backfill complete: {} signature(s) indexed, {} skipped as failed-on-chain, {} window(s), stop={:?}",
        summary.signatures_expected, summary.signatures_failed, summary.windows, summary.stop
    );
    Ok(())
}

async fn run_snapshot(serve_metrics: bool) -> Result<()> {
    let started = start().await?;
    if serve_metrics {
        if let Err(e) = indexer::metrics::install(started.cfg.metrics_addr) {
            log::warn!("continuing without a metrics listener: {e}");
        }
    }

    let shutdown = CancellationToken::new();
    spawn_ctrl_c_watcher(shutdown.clone());

    // Standalone: no live stream is running in *this* process, so the ordering guarantee of
    // spec §7 does not apply here -- the operator is either seeding a database before starting
    // the indexer (in which case `run` will re-snapshot only if snapshot_slot is unset) or
    // repairing state next to a running indexer, whose stream is already up.
    let summary = snapshot::run(
        &started.cfg,
        &started.pool,
        &started.tracked,
        shutdown.clone(),
    )
    .await?;

    println!(
        "snapshot complete: {} account(s) written at slot {}{}",
        summary.accounts_loaded,
        summary.slot,
        if summary.undecodable > 0 {
            format!(", {} undecodable", summary.undecodable)
        } else {
            String::new()
        }
    );
    Ok(())
}

async fn run_smoke(timeout: Duration) -> Result<()> {
    let cfg = Config::from_env()?;
    match indexer::grpc_smoke::run(&cfg, timeout).await {
        Ok(first) => {
            println!("gRPC OK (first update: {first})");
            Ok(())
        }
        Err(e) => {
            eprintln!("gRPC FAILED: {e:#}");
            Err(e)
        }
    }
}

fn spawn_ctrl_c_watcher(token: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            log::info!("SIGINT received; shutting down");
            token.cancel();
        }
    });
}
