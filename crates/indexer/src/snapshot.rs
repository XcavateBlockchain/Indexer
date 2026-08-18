//! The `getProgramAccounts` state snapshots (spec §7, ruling R13), one per program.
//!
//! The account-state tables can only be filled by the live account stream, and the stream
//! only fires when an account *changes* -- on programs that are idle for days that is never.
//! So a fresh database needs one bulk read of current state per program: `getSlot` ->
//! `getProgramAccounts(<program>)` -> the same decoder and the same row mapping the account
//! pipes use (via the registry's `snapshot_write_op` dispatch) -> the same slot-guarded
//! upserts.
//!
//! ## Why this is a plain loop and not a carbon `Datasource`
//!
//! Ruling R13: carbon 0.12.0 ships no gPA datasource, so it would have to be written here
//! either way. As a `Datasource` it would have to fabricate `AccountUpdate`s, be driven by a
//! whole `Pipeline`, and then be waited on with the same "when is it done?" problem the crawler
//! has. As a loop it is ~40 lines, reuses each program's `account_write_op` verbatim (so a
//! snapshot row and a stream row are byte-identical by construction), and finishes when the
//! `for` loop finishes. The batcher is still the only writer.
//!
//! ## Ordering: the stream is started FIRST, then the snapshot is taken
//!
//! This is the non-negotiable part of spec §7 and it looks like removable complexity if you do
//! not know the failure mode, so: a `getProgramAccounts` call takes a while, and any account
//! that changes between the snapshot's read and the stream's subscription is invisible to both
//! -- the snapshot has the pre-change value and the stream never saw the change. That is a
//! permanent gap exactly as wide as the snapshot. Subscribing first and snapshotting second
//! makes the two overlap instead, and the slot guard resolves the overlap: anything the stream
//! already delivered at a higher slot survives the snapshot's upsert untouched.
//!
//! The snapshot is tagged with the slot read *before* the gPA call, so the tag can only be
//! older than (never newer than) the state it describes -- again the direction the slot guard
//! forgives.

use std::time::Duration;

use anyhow::{Context, Result};
use solana_account::Account;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{
    RpcAccountInfoConfig, RpcProgramAccountsConfig, UiAccountEncoding,
};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::batcher;
use crate::config::{redact_key, Config};
use crate::db;
use crate::processors::TrackedAccounts;
use crate::programs::ProgramSpec;

#[derive(Debug, Clone, Copy)]
pub struct SnapshotSummary {
    /// Slot the snapshot is tagged with (read before the `getProgramAccounts` call).
    pub slot: u64,
    pub accounts_loaded: usize,
    /// Accounts owned by the program that the decoder did not recognise. Never expected; a
    /// non-zero count means the deployed program has an account type the checked-in IDL lacks.
    pub undecodable: usize,
    /// Still-open state rows whose accounts were absent from the snapshot and were therefore
    /// swept closed (see `db::close::close_missing_in_table`) -- the healing path for closes
    /// nothing else could land.
    pub closed_missing: u64,
}

/// Take one program's snapshot and commit it. Returns once every row is committed and that
/// program's `sync_state.snapshot_slot` is set.
pub async fn run(
    cfg: &Config,
    program: &'static ProgramSpec,
    pool: &PgPool,
    tracked: &TrackedAccounts,
    shutdown: CancellationToken,
) -> Result<SnapshotSummary> {
    let (slot, accounts) = fetch(cfg, program).await?;
    log::info!(
        "snapshot[{}]: getProgramAccounts returned {} account(s) at slot {slot}",
        program.name,
        accounts.len()
    );

    // A batcher of its own: dropping it and awaiting the flusher is the commit barrier that
    // makes it safe to write `snapshot_slot` afterwards.
    let (bat, flusher) = batcher::spawn(pool.clone(), shutdown.clone());

    let mut loaded = 0usize;
    let mut undecodable = 0usize;
    for (pubkey, account) in &accounts {
        let Some(op) =
            (program.snapshot_write_op)(*pubkey, slot as i64, account.lamports as i64, account)
        else {
            undecodable += 1;
            log::error!(
                "snapshot[{}]: account {pubkey} is owned by the program but did not decode \
                 ({} bytes); skipping it",
                program.name,
                account.data.len()
            );
            continue;
        };
        // Every snapshotted PDA becomes deletion-tracked, so a later close reaches the deletion
        // pipe instead of being dropped by the Yellowstone datasource.
        tracked.write().await.insert(*pubkey);
        bat.push(op)
            .await
            .context("snapshot: batcher channel closed")?;
        loaded += 1;
    }

    drop(bat);
    // FINDING 2 (Task-4 fix round): awaiting the flusher is a commit barrier only if it reports
    // one. A panicked flusher task is indistinguishable from a dropped batch here, so it is
    // treated the same conservative way.
    let flush_outcome = flusher.await.unwrap_or(batcher::FlushOutcome::OpsDropped);
    if !flush_outcome.all_committed() {
        // Fail before the sweep: with the snapshot's own upserts not all committed, "absent
        // from the snapshot" is not yet evidence the caller can act on. finish() repeats the
        // check for its own write; the early bail here just keeps the sweep behind the same
        // barrier.
        return Err(
            finish(pool, &program.id.to_bytes(), flush_outcome, slot, loaded)
                .await
                .expect_err("finish must reject a dropped flush"),
        );
    }

    // The close-missing sweep: every still-open row whose account was not in this snapshot
    // is provably closed on-chain (the strict slot guards cannot land some closes -- see
    // db::close). Runs after the commit barrier above, so the upserts it reasons against are
    // all durable; each UPDATE is itself slot-guarded and idempotent.
    let live: Vec<Vec<u8>> = accounts
        .iter()
        .map(|(pubkey, _)| pubkey.to_bytes().to_vec())
        .collect();
    let mut closed_missing = 0u64;
    for table in program.tables {
        let result = db::close::close_missing_in_table(pool, *table, &live, slot as i64)
            .await
            .with_context(|| {
                format!(
                    "snapshot: close-missing sweep failed for {}",
                    table.table_name()
                )
            })?;
        closed_missing += result.rows_affected();
    }
    if closed_missing > 0 {
        log::warn!(
            "snapshot[{}]: swept {closed_missing} stale-open row(s) closed (their accounts \
             are gone on-chain but no instruction-driven or deletion-pipe close had landed)",
            program.name
        );
    }

    finish(pool, &program.id.to_bytes(), flush_outcome, slot, loaded).await?;
    crate::metrics::set_snapshot_accounts_loaded(program.name, loaded as u64);

    log::info!(
        "snapshot[{}]: complete -- {loaded} account(s) written at slot {slot}, snapshot_slot \
         recorded",
        program.name
    );
    Ok(SnapshotSummary {
        slot,
        accounts_loaded: loaded,
        undecodable,
        closed_missing,
    })
}

/// Records one program's `sync_state.snapshot_slot`, unless the flusher had to drop a batch (a
/// double fault: a commit kept failing and shutdown fired during its retry backoff -- see
/// `crate::batcher::flush`). Split out of `run` so FINDING 2 (Task-4 fix round) is
/// unit-testable without a live RPC fetch: writing `snapshot_slot` here would claim every one
/// of `loaded` accounts landed in the state tables, which is exactly the false claim a dropped
/// batch would otherwise let through. A hard error, loud and resumable: `indexer snapshot` is
/// a plain idempotent re-read of current chain state.
async fn finish(
    pool: &PgPool,
    program_id: &[u8],
    flush_outcome: batcher::FlushOutcome,
    slot: u64,
    loaded: usize,
) -> Result<()> {
    if !flush_outcome.all_committed() {
        anyhow::bail!(
            "snapshot: {loaded} account write(s) were dropped uncommitted during a double \
             fault (DB commit failure + shutdown); refusing to record sync_state.snapshot_slot. \
             Re-run `indexer snapshot` -- it is a plain idempotent re-read of current chain \
             state."
        );
    }
    db::sync_state::set_snapshot_slot(pool, program_id, slot as i64)
        .await
        .context("snapshot: recording sync_state.snapshot_slot")?;
    Ok(())
}

/// `getSlot` then `getProgramAccounts`, on the primary endpoint with the public fallback behind
/// it (Alchemy's free tier throttles -- see MIGRATION_LOG.md).
async fn fetch(cfg: &Config, program: &ProgramSpec) -> Result<(u64, Vec<(Pubkey, Account)>)> {
    let primary = cfg.rpc_url();
    let mut last_err = None;
    for (label, url) in [
        ("primary", primary.as_str()),
        ("fallback", cfg.rpc_fallback_url.as_str()),
    ] {
        match fetch_from(url, &program.id).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                // Never log the raw error: the Alchemy JSON-RPC endpoint carries the API key in
                // its path, and reqwest/solana-rpc-client's Error Display can append it via
                // " for url (<url>)" -- redact before logging.
                log::warn!(
                    "snapshot[{}]: {label} RPC failed ({}); trying the next endpoint",
                    program.name,
                    redact_key(&e.to_string())
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no RPC endpoint configured")))
}

async fn fetch_from(url: &str, program_id: &Pubkey) -> Result<(u64, Vec<(Pubkey, Account)>)> {
    let rpc = RpcClient::new_with_timeout_and_commitment(
        url.to_string(),
        Duration::from_secs(60),
        CommitmentConfig::confirmed(),
    );

    // Recorded BEFORE the read (see the module docs): the tag must never claim to be newer than
    // the state it describes.
    let slot = rpc
        .get_slot_with_commitment(CommitmentConfig::confirmed())
        .await
        .context("getSlot failed")?;

    // The `_ui_` variant rather than `get_program_accounts_with_config`: the latter is
    // deprecated in solana-rpc-client 3.1, and decoding the base64 payload here ourselves makes
    // the encoding assumption explicit (and an unexpected encoding a loud error, not a panic).
    let ui_accounts = rpc
        .get_program_ui_accounts_with_config(
            program_id,
            RpcProgramAccountsConfig {
                // No memcmp/dataSize filters: the decoder discriminates by discriminator, and a
                // server-side filter would silently drop any account type added to the program
                // later. The same reasoning as the live stream's owner-only account filter.
                filters: None,
                account_config: RpcAccountInfoConfig {
                    encoding: Some(UiAccountEncoding::Base64),
                    data_slice: None,
                    commitment: Some(CommitmentConfig::confirmed()),
                    min_context_slot: None,
                },
                with_context: None,
                sort_results: None,
            },
        )
        .await
        .context("getProgramAccounts failed")?;

    let mut accounts = Vec::with_capacity(ui_accounts.len());
    for (pubkey, ui) in ui_accounts {
        let account: Account = ui.decode().with_context(|| {
            format!("getProgramAccounts returned {pubkey} in an undecodable encoding")
        })?;
        accounts.push((pubkey, account));
    }
    Ok((slot, accounts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batcher::FlushOutcome;

    const PID: &[u8] = &[7u8; 32];

    #[sqlx::test(migrations = "../../migrations")]
    async fn dropped_ops_skip_recording_snapshot_slot(pool: PgPool) {
        db::sync_state::init_sync_state(&pool, PID, 100)
            .await
            .unwrap();

        let err = finish(&pool, PID, FlushOutcome::OpsDropped, 999, 11)
            .await
            .expect_err("a double-fault flush must be a hard error");
        assert!(err.to_string().to_lowercase().contains("dropped"));

        let state = db::sync_state::get_sync_state(&pool, PID)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state.snapshot_slot, None,
            "snapshot_slot must stay unset when the flush dropped ops"
        );
    }

    #[sqlx::test(migrations = "../../migrations")]
    async fn a_clean_flush_records_snapshot_slot(pool: PgPool) {
        db::sync_state::init_sync_state(&pool, PID, 100)
            .await
            .unwrap();

        finish(&pool, PID, FlushOutcome::AllCommitted, 999, 11)
            .await
            .unwrap();

        let state = db::sync_state::get_sync_state(&pool, PID)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.snapshot_slot, Some(999));
    }
}
