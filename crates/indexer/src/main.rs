//! The indexer binary.
//!
//! ```text
//! indexer run          # live Yellowstone gRPC pipeline (production)
//! indexer replay       # crawl the program's whole signature history through the same pipes
//! indexer smoke-grpc   # connectivity/auth/filter check against the Yellowstone endpoint
//! indexer backfill     # (Task 4)
//! indexer snapshot     # (Task 4)
//! ```
//!
//! Configuration is entirely environmental -- see `indexer::config`. Load the repo `.env` into
//! the shell first (`set -a; . ./.env; set +a`): the binary deliberately does not read a dotenv
//! file itself, so the process sees exactly what the operator (or the container runtime)
//! exported, with no third source of truth to reconcile.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use carbon_core::error::CarbonResult;
use carbon_core::metrics::Metrics;
use clap::{Parser, Subcommand};
use indexer::batcher;
use indexer::block_time::BlockTimeResolver;
use indexer::config::{redact_url_password, Config};
use indexer::db;
use indexer::metrics::PrometheusMetrics;
use indexer::pipeline::{self, PipeDeps};
use indexer::processors::SessionMarker;
use indexer::sync_frontier::SyncFrontier;
use solana_pubkey::Pubkey;
use tokio_util::sync::CancellationToken;

/// gRPC reconnect backoff bounds.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A stream session that lasted at least this long counts as healthy, and resets the backoff.
const HEALTHY_SESSION: Duration = Duration::from_secs(120);

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
    /// Run the live Yellowstone gRPC pipeline.
    Run,
    /// Replay the program's full on-chain history through the same pipes using the RPC
    /// transaction crawler. Exits once no new transaction has arrived for `--idle-timeout`.
    Replay {
        /// Seconds without a new update before the crawl is considered finished.
        #[arg(long, default_value_t = 20)]
        idle_timeout: u64,
        /// Serve `/metrics` during the replay. Off by default so a replay can run alongside a
        /// live indexer without fighting over the port.
        #[arg(long)]
        metrics: bool,
    },
    /// Connect to the Yellowstone endpoint, subscribe with the production filters plus a slot
    /// heartbeat, and report the first update.
    SmokeGrpc {
        /// Seconds to wait for the first update.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Historical backfill (Task 4).
    Backfill,
    /// `getProgramAccounts` state snapshot (Task 4).
    Snapshot,
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
        Command::Replay {
            idle_timeout,
            metrics,
        } => run_replay(Duration::from_secs(idle_timeout), metrics).await,
        Command::SmokeGrpc { timeout } => run_smoke(Duration::from_secs(timeout)).await,
        Command::Backfill | Command::Snapshot => {
            anyhow::bail!("not implemented yet -- wired in Task 4 (phase 4: backfill + snapshot)")
        }
    }
}

/// Shared startup: migrations, sync-state seeding, and the pieces every pipeline needs.
struct Started {
    cfg: Config,
    pool: sqlx::PgPool,
    frontier: Arc<SyncFrontier>,
    block_time: Arc<BlockTimeResolver>,
    tracked: indexer::processors::TrackedAccounts,
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
    if state.backfill_complete {
        log::info!(
            "backfill is marked complete, but a freshly started process is by definition behind \
             the chain tip: the frontier starts with a gap open, so last_contiguous_slot stays \
             at {} until a catch-up backfill calls SyncFrontier::gap_closed (Task 4)",
            state.last_contiguous_slot
        );
    } else {
        log::warn!(
            "backfill has not completed; last_contiguous_slot will stay at {} until Task 4's \
             backfill runs and closes the gap",
            state.last_contiguous_slot
        );
    }

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
    })
}

async fn run_live() -> Result<()> {
    let started = start().await?;
    let cfg = &started.cfg;
    let api_key = cfg.require_api_key()?.to_string();

    indexer::metrics::install(cfg.metrics_addr)?;

    let shutdown = CancellationToken::new();
    spawn_ctrl_c_watcher(shutdown.clone());

    let (batcher, flusher) = batcher::spawn(
        started.pool.clone(),
        started.frontier.clone(),
        shutdown.clone(),
    );

    let mut backoff = RECONNECT_BACKOFF_MIN;
    while !shutdown.is_cancelled() {
        // A fresh marker per session: the "stream connected at slot S" line and the frontier's
        // session arming both have to happen again after a reconnect.
        let session = SessionMarker::new(started.frontier.clone());
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
            session,
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
        // meantime is a hole, and the frontier must freeze until a catch-up backfill fills it.
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

    // Dropping the last Batcher closes the channel, which makes the flusher commit its final
    // partial batch and exit.
    drop(batcher);
    flusher.await.ok();
    log::info!("shutdown complete");
    Ok(())
}

async fn run_replay(idle_timeout: Duration, serve_metrics: bool) -> Result<()> {
    let started = start().await?;
    let cfg = &started.cfg;

    if serve_metrics {
        if let Err(e) = indexer::metrics::install(cfg.metrics_addr) {
            log::warn!("continuing without a metrics listener: {e}");
        }
    }

    let rpc_url = cfg.rpc_url();
    log::info!(
        "replaying the full signature history of {} via {} (idle timeout {:?})",
        cfg.program_id,
        // Never log the URL itself: the Alchemy JSON-RPC endpoint carries the key in its path.
        if rpc_url == cfg.rpc_fallback_url {
            cfg.rpc_fallback_url.as_str()
        } else {
            "<primary RPC>"
        },
        idle_timeout,
    );

    let shutdown = CancellationToken::new();
    spawn_ctrl_c_watcher(shutdown.clone());

    let (batcher, flusher) = batcher::spawn(
        started.pool.clone(),
        started.frontier.clone(),
        shutdown.clone(),
    );

    let ds_cancel = shutdown.child_token();
    let activity = Arc::new(ActivityMetrics::default());

    // The activity-tracking metrics wrapper is how the watchdog tells when the crawler has run
    // out of history (the crawler polls forever by design; it never signals "done").
    let mut pipe = pipeline::build_replay(
        cfg,
        &rpc_url,
        PipeDeps {
            batcher: &batcher,
            block_time: &started.block_time,
            tracked: &started.tracked,
            metrics: activity.clone(),
        },
        ds_cancel.clone(),
    )
    .map_err(|e| anyhow::anyhow!("building the replay pipeline failed: {e}"))?;

    let watchdog = spawn_idle_watchdog(activity.clone(), ds_cancel.clone(), idle_timeout);

    let outcome = pipe.run().await;
    drop(pipe);
    ds_cancel.cancel();
    watchdog.await.ok();

    drop(batcher);
    flusher.await.ok();

    match outcome {
        Ok(()) => log::info!(
            "replay finished: {} updates received",
            activity.updates.load(Ordering::Relaxed)
        ),
        Err(e) => anyhow::bail!("replay pipeline failed: {e}"),
    }
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

/// Cancels `token` once `activity` has been quiet for `idle_timeout`.
///
/// The RPC crawler has no completion signal -- after exhausting history it keeps polling for
/// new signatures forever -- so "no updates for a while" is the only available end condition
/// for a one-shot replay.
fn spawn_idle_watchdog(
    activity: Arc<ActivityMetrics>,
    token: CancellationToken,
    idle_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = ticker.tick() => {
                    let idle = activity.idle_for();
                    if idle >= idle_timeout {
                        log::info!(
                            "no updates for {idle:?}; crawl considered complete ({} updates seen)",
                            activity.updates.load(Ordering::Relaxed)
                        );
                        token.cancel();
                        return;
                    }
                }
            }
        }
    })
}

/// A `Metrics` implementation that forwards to Prometheus and additionally records when the
/// pipeline last saw an update, for the replay watchdog.
struct ActivityMetrics {
    inner: PrometheusMetrics,
    updates: AtomicU64,
    /// Milliseconds since `origin`, so the watchdog needs no clock syscall per tick.
    last_activity_ms: AtomicU64,
    origin: std::time::Instant,
}

impl Default for ActivityMetrics {
    fn default() -> Self {
        Self {
            inner: PrometheusMetrics,
            updates: AtomicU64::new(0),
            last_activity_ms: AtomicU64::new(0),
            origin: std::time::Instant::now(),
        }
    }
}

impl ActivityMetrics {
    fn idle_for(&self) -> Duration {
        let now = self.origin.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_activity_ms.load(Ordering::Relaxed)))
    }

    fn touch(&self) {
        self.last_activity_ms
            .store(self.origin.elapsed().as_millis() as u64, Ordering::Relaxed);
    }
}

#[async_trait]
impl Metrics for ActivityMetrics {
    async fn initialize(&self) -> CarbonResult<()> {
        self.inner.initialize().await
    }
    async fn flush(&self) -> CarbonResult<()> {
        self.inner.flush().await
    }
    async fn shutdown(&self) -> CarbonResult<()> {
        self.inner.shutdown().await
    }
    async fn update_gauge(&self, name: &str, value: f64) -> CarbonResult<()> {
        self.inner.update_gauge(name, value).await
    }
    async fn increment_counter(&self, name: &str, value: u64) -> CarbonResult<()> {
        if name == "updates_received" {
            self.updates.fetch_add(value, Ordering::Relaxed);
            self.touch();
        }
        self.inner.increment_counter(name, value).await
    }
    async fn record_histogram(&self, name: &str, value: f64) -> CarbonResult<()> {
        self.inner.record_histogram(name, value).await
    }
}
