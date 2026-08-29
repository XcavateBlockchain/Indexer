//! The batched writer: every database write in the indexer goes through here.
//!
//! Processors must not hold the pipeline open for the length of a database round trip -- one
//! `process()` call per instruction, each opening its own transaction, would serialise the
//! whole stream behind Postgres. So processors only push typed [`WriteOp`]s into an mpsc
//! channel and return; a single flusher task drains that channel and commits a batch in one
//! transaction, either when it has [`MAX_BATCH`] ops or after [`MAX_INTERVAL`], whichever
//! comes first. The bounded channel is the backpressure: if Postgres falls behind, `push`
//! blocks the processor, which blocks carbon's update loop, which is exactly what we want
//! (rather than an unbounded queue that grows until the process dies).
//!
//! Ordering inside the transaction is fixed (accounts -> instructions -> actions -> closes ->
//! backfill cursor) so that a close always lands after the upsert it is closing, and the
//! backfill's resume cursor after the rows it vouches for, even when they arrive in the same
//! batch. Every individual write is idempotent and slot-guarded by Task 2's db module, so
//! ordering *between* batches never matters -- this ordering only removes the within-batch race.
//!
//! Note what this module does NOT write: `sync_state.last_contiguous_slot`. Task 3 advanced it
//! from here (max slot of a committed batch, gated by the sync frontier); Task 4 moved that to
//! the reconciliation supervisor, because a committed batch is evidence that *these* rows
//! landed, never that nothing between them was missed. See [`crate::sync_frontier`].

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db;
use crate::db::close::StateTable;
use crate::db::models::{AdminAccount, ConfigAccount, NewAction, NewInstruction, RoleAccountRow};

/// Flush when this many ops are buffered.
pub const MAX_BATCH: usize = 100;
/// ...or this long after the first op of a batch, whichever comes first.
pub const MAX_INTERVAL: Duration = Duration::from_millis(250);
/// Channel depth. Deep enough that a normal flush never blocks a processor, shallow enough
/// that a stalled database applies backpressure within a few batches.
pub const CHANNEL_CAPACITY: usize = 4 * MAX_BATCH;

// A channel shallower than one batch could not even hold a full batch before the flusher wakes
// up, which would turn every batch into a stop-and-go.
const _: () = assert!(CHANNEL_CAPACITY > MAX_BATCH);

/// One database write. Deliberately a closed enum rather than a boxed closure so the flusher
/// can group by kind and so the whole op stream is inspectable/testable.
#[derive(Debug, Clone)]
pub enum WriteOp {
    UpsertConfig(ConfigAccount),
    UpsertAdmin(AdminAccount),
    UpsertRoleAccount(RoleAccountRow),
    UpsertRegionsAccount(db::regions::RegionsAccountRow),
    UpsertMarketplaceAccount(db::marketplace::MarketplaceAccountRow),
    UpsertPropertyAccount(db::property::PropertyAccountRow),
    UpsertRealxhubAccount(db::realxhub::RealxhubAccountRow),
    InsertInstruction(NewInstruction),
    InsertAction(NewAction),
    /// Instruction-driven close of a PDA in a known state table (ruling R11).
    CloseAccount {
        table: StateTable,
        pubkey: Vec<u8>,
        slot: i64,
    },
    /// A conditional close the mapper cannot decide (it is pure, no DB), made by the write
    /// itself against the stored row: property's `remove_letting_agent` closes the
    /// `LettingAgent` PDA on-chain only when the removed location was its last -- see
    /// `db::property::close_letting_agent_if_last`.
    CloseLettingAgentIfLast {
        pubkey: Vec<u8>,
        /// The removed location's postcode as a UTF-8 string, matching the shape the
        /// `locations` JSONB stores.
        removed_postcode: String,
        slot: i64,
    },
    /// Conditional close of a `ShareListing` by `buy_relisted_shares` (on-chain: closed only
    /// when the buy emptied it) -- see `db::marketplace::close_share_listing_if_emptied`.
    CloseShareListingIfEmptied {
        pubkey: Vec<u8>,
        /// The instruction's `amount` arg: how many shares were bought.
        bought_amount: i64,
        slot: i64,
    },
    /// Conditional close of a `ShareListing` by `accept_offer`, whose sold amount is the
    /// offer account's amount rather than an instruction arg -- see
    /// `db::marketplace::close_share_listing_if_emptied_by_offer`.
    CloseShareListingIfEmptiedByOffer {
        pubkey: Vec<u8>,
        offer_pubkey: Vec<u8>,
        slot: i64,
    },
    /// Conditional close of a realxhub `ShareListing` by `buy_shares` (on-chain: closed only
    /// when the buy drained the listing's remaining shares to zero) -- see
    /// `db::realxhub::close_share_listing_if_emptied`.
    CloseRealxhubShareListingIfEmptied {
        pubkey: Vec<u8>,
        /// The instruction's `amount` arg: how many shares were bought.
        bought_amount: i64,
        slot: i64,
    },
    /// Close driven by carbon's `AccountDeletion`, which carries only `{pubkey, slot}` and so
    /// cannot say which table the pubkey belongs to. Tries every state table; each is a
    /// guarded `UPDATE ... WHERE pubkey = $1 AND slot < $2`, so the ones that do not match
    /// are no-ops.
    CloseUnknownAccount {
        pubkey: Vec<u8>,
        slot: i64,
    },
    /// One program's history-backfill resume cursor: "every transaction of this program at or
    /// above this signature has been committed". Pushed *after* the rows of the page it
    /// describes, and sorted last within its batch, so it can never be committed ahead of
    /// them -- if the process dies in between, the cursor is simply one page stale and that
    /// page is walked again.
    SetBackfillCursor {
        program_id: Vec<u8>,
        signature: String,
        slot: i64,
    },
    /// One observed BPFLoaderUpgradeable `Upgrade` of a registry program (ADR-24), from the
    /// recorder pipe in [`crate::upgrades`]. Idempotent like every append -- crawl re-walks
    /// re-deliver historical upgrade transactions -- and only the first commit of a given
    /// (program, slot) counts as a *detection*: the flusher bumps the detection metric and
    /// warn-logs only for rows that were actually new, and only after their transaction
    /// committed (a metric bumped inside `commit_batch` would double-count on a retried
    /// commit and could count a rolled-back row). This makes the side effects AT MOST
    /// once, not exactly once: a commit that succeeded on the server but errored on the
    /// wire is retried, the retry's insert is an `ON CONFLICT` no-op, and the metric/log
    /// are skipped for a row that did land. Acceptable by design -- the durable record is
    /// the `program_upgrades` row itself (which always lands), the alert window is wide,
    /// and `scripts/agent/check-program-upgrades.py` probes the chain independently.
    RecordProgramUpgrade {
        /// Registry name, for the detection metric's `program` label.
        program: &'static str,
        program_id: Vec<u8>,
        upgrade_slot: i64,
        signature: String,
    },
    /// A durable webhook event to deliver (ADR-28), emitted by the marketplace mapper for
    /// `init_property_assets` (a new property asset registered). Committed as
    /// `INSERT ... ON CONFLICT (event_id) DO NOTHING` -- idempotent, so a backfill re-walk
    /// re-delivering the same instruction is a no-op and the notification is recorded at most
    /// once. The row is both the durable "this event happened" record and the delivery queue:
    /// the background loop ([`crate::webhooks`]) reads the undelivered rows and POSTs each
    /// `payload` to `WEBHOOK_URL`, stamping the delivery timestamps (at-least-once delivery
    /// with per-event backoff).
    RecordWebhookEvent {
        /// `<event_type>:<base58 subject key>` -- the `webhook_events` primary key.
        event_id: String,
        /// Low-cardinality event label (`property_asset_registered`).
        event_type: &'static str,
        /// The JSON document the delivery loop POSTs to `WEBHOOK_URL`.
        payload: serde_json::Value,
        /// Slot of the transaction that produced the event (provenance).
        slot: i64,
        /// base58 signature of that transaction.
        tx_signature: String,
        /// The transaction's block time.
        block_time: DateTime<Utc>,
    },
}

/// A `RecordProgramUpgrade` row that `commit_batch` actually inserted (as opposed to one
/// deduplicated by `ON CONFLICT DO NOTHING`), reported back to `flush` so the detection
/// side effects fire at most once per boundary, after the commit (see the variant's doc
/// for why "at most" and why that is fine).
struct NewUpgrade {
    program: &'static str,
    upgrade_slot: i64,
    signature: String,
}

impl WriteOp {
    /// The slot this write is evidence for. Logged per flush, and used as the
    /// `backfill_last_processed_slot` gauge value while a history walk is running.
    pub fn slot(&self) -> i64 {
        match self {
            WriteOp::UpsertConfig(r) => r.slot,
            WriteOp::UpsertAdmin(r) => r.slot,
            WriteOp::UpsertRoleAccount(r) => r.slot,
            WriteOp::UpsertRegionsAccount(r) => r.slot(),
            WriteOp::UpsertMarketplaceAccount(r) => r.slot(),
            WriteOp::UpsertPropertyAccount(r) => r.slot(),
            WriteOp::UpsertRealxhubAccount(r) => r.slot(),
            WriteOp::InsertInstruction(r) => r.slot,
            WriteOp::InsertAction(r) => r.slot,
            WriteOp::CloseAccount { slot, .. }
            | WriteOp::CloseLettingAgentIfLast { slot, .. }
            | WriteOp::CloseShareListingIfEmptied { slot, .. }
            | WriteOp::CloseShareListingIfEmptiedByOffer { slot, .. }
            | WriteOp::CloseRealxhubShareListingIfEmptied { slot, .. }
            | WriteOp::CloseUnknownAccount { slot, .. }
            | WriteOp::SetBackfillCursor { slot, .. } => *slot,
            WriteOp::RecordProgramUpgrade { upgrade_slot, .. } => *upgrade_slot,
            WriteOp::RecordWebhookEvent { slot, .. } => *slot,
        }
    }

    /// Position of this op's kind in the within-transaction ordering (see module docs).
    fn phase(&self) -> u8 {
        match self {
            WriteOp::UpsertConfig(_)
            | WriteOp::UpsertAdmin(_)
            | WriteOp::UpsertRoleAccount(_)
            | WriteOp::UpsertRegionsAccount(_)
            | WriteOp::UpsertMarketplaceAccount(_)
            | WriteOp::UpsertPropertyAccount(_)
            | WriteOp::UpsertRealxhubAccount(_)
            // Orders against nothing: program_upgrades and webhook_events share no rows with
            // any other op kind (both are idempotent append-only records).
            | WriteOp::RecordProgramUpgrade { .. }
            | WriteOp::RecordWebhookEvent { .. } => 0,
            WriteOp::InsertInstruction(_) => 1,
            WriteOp::InsertAction(_) => 2,
            WriteOp::CloseAccount { .. }
            | WriteOp::CloseLettingAgentIfLast { .. }
            | WriteOp::CloseShareListingIfEmptied { .. }
            | WriteOp::CloseShareListingIfEmptiedByOffer { .. }
            | WriteOp::CloseRealxhubShareListingIfEmptied { .. }
            | WriteOp::CloseUnknownAccount { .. } => 3,
            WriteOp::SetBackfillCursor { .. } => 4,
        }
    }
}

/// Cheap-to-clone handle the processors hold.
#[derive(Clone, Debug)]
pub struct Batcher {
    tx: mpsc::Sender<WriteOp>,
}

impl Batcher {
    /// Pushes one op, waiting if the channel is full (this is the backpressure path).
    /// Errors only once the flusher has shut down and the receiver is gone.
    pub async fn push(&self, op: WriteOp) -> Result<(), mpsc::error::SendError<()>> {
        self.tx
            .send(op)
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    pub async fn push_many(
        &self,
        ops: impl IntoIterator<Item = WriteOp>,
    ) -> Result<(), mpsc::error::SendError<()>> {
        for op in ops {
            self.tx
                .send(op)
                .await
                .map_err(|_| mpsc::error::SendError(()))?;
        }
        Ok(())
    }
}

/// Whether every write op ever pushed through a [`Batcher`] before its flusher exited actually
/// committed. Returned by the [`JoinHandle`] `spawn` hands back, so the one-shot jobs (snapshot,
/// history backfill, one reconciliation cycle) can tell a real commit barrier from a laundered
/// failure before writing a completion marker that depends on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushOutcome {
    /// Every batch this flusher ever committed, committed. Safe to trust the rows it wrote.
    AllCommitted,
    /// At least one batch was dropped uncommitted -- a commit kept failing and the
    /// cancellation token fired during its retry backoff (see [`flush`]). The rows in that
    /// batch (and, since ordering within a batch is fixed, anything sorted after them --
    /// notably a backfill cursor sharing the batch) never landed. A caller must NOT write any
    /// completion marker (`backfill_complete`, a cursor clear, `snapshot_slot`,
    /// `last_contiguous_slot`) that depends on this flusher's rows having committed.
    OpsDropped,
}

impl FlushOutcome {
    pub fn all_committed(self) -> bool {
        matches!(self, FlushOutcome::AllCommitted)
    }
}

/// Creates the channel and spawns the flusher.
///
/// The returned [`JoinHandle`] completes once every [`Batcher`] clone has been dropped *and*
/// the final partial batch has been committed or dropped -- so a graceful shutdown is: drop the
/// pipeline (which drops the processors, which drop their `Batcher`s), then await this handle.
///
/// That property is also used as a **commit barrier** by the one-shot jobs (snapshot, history
/// backfill, one reconciliation cycle): each creates its own batcher, does its work, drops the
/// handle and awaits the flusher. The [`FlushOutcome`] it resolves to says whether that barrier
/// actually held: `AllCommitted` means everything the job produced landed, so it is safe to
/// then write `snapshot_slot`, `backfill_complete` or `last_contiguous_slot`; `OpsDropped` means
/// a commit kept failing until shutdown fired during its retry backoff, and the caller MUST
/// skip those completion writes -- they would otherwise claim completeness for rows that never
/// landed (Task-4 fix round, Finding 2).
pub fn spawn(pool: PgPool, cancellation: CancellationToken) -> (Batcher, JoinHandle<FlushOutcome>) {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let handle = tokio::spawn(flusher_loop(pool, rx, cancellation));
    (Batcher { tx }, handle)
}

async fn flusher_loop(
    pool: PgPool,
    mut rx: mpsc::Receiver<WriteOp>,
    cancellation: CancellationToken,
) -> FlushOutcome {
    let mut buf: Vec<WriteOp> = Vec::with_capacity(MAX_BATCH);
    // `None` while the buffer is empty: an idle flusher must not wake up every 250 ms.
    let mut deadline: Option<tokio::time::Instant> = None;
    // Sticky once true: one dropped batch during this flusher's lifetime is enough to make its
    // overall report `OpsDropped`, even if later flushes (there might not be any -- shutdown is
    // usually imminent once this happens) succeed.
    let mut dropped_any = false;

    loop {
        let timer = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                // Never completes; `select!` just waits on the channel instead.
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            biased;

            received = rx.recv() => match received {
                Some(op) => {
                    if buf.is_empty() {
                        deadline = Some(tokio::time::Instant::now() + MAX_INTERVAL);
                    }
                    buf.push(op);
                    if buf.len() >= MAX_BATCH {
                        dropped_any |= flush(&pool, &mut buf, &cancellation).await;
                        deadline = None;
                    }
                }
                None => {
                    // All senders dropped: commit whatever is left and finish.
                    dropped_any |= flush(&pool, &mut buf, &cancellation).await;
                    log::info!("batch flusher stopped (all writers dropped)");
                    return if dropped_any {
                        FlushOutcome::OpsDropped
                    } else {
                        FlushOutcome::AllCommitted
                    };
                }
            },

            _ = timer => {
                dropped_any |= flush(&pool, &mut buf, &cancellation).await;
                deadline = None;
            }
        }
    }
}

/// Commits `buf` in one transaction, retrying with exponential backoff until it succeeds or
/// the process is cancelled. `buf` is left empty either way. Returns `true` if `buf` had to be
/// dropped uncommitted (cancelled while retrying a failed commit) -- the caller accumulates
/// this into the [`FlushOutcome`] the flusher eventually reports.
async fn flush(pool: &PgPool, buf: &mut Vec<WriteOp>, cancellation: &CancellationToken) -> bool {
    if buf.is_empty() {
        return false;
    }

    // Stable sort by phase: within a phase the original arrival order is preserved, which
    // keeps the ordering of two writes to the same row deterministic.
    buf.sort_by_key(WriteOp::phase);

    let max_slot = buf.iter().map(WriteOp::slot).max().unwrap_or(0);

    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match commit_batch(pool, buf).await {
            Ok(new_upgrades) => {
                let elapsed = started.elapsed();
                crate::metrics::record_flush(elapsed, buf.len());
                // Detection side effects for newly-recorded upgrade boundaries, strictly after
                // the commit (see `WriteOp::RecordProgramUpgrade`). warn! because this is the
                // one write that means "the deployed program and the checked-in IDL may have
                // diverged" -- the ProgramUpgradeDetected alert fires off the same counter,
                // and RUNBOOK.md "After a program upgrade" is the follow-up.
                for up in &new_upgrades {
                    crate::metrics::inc_program_upgrade_detected(up.program);
                    log::warn!(
                        "NEW program upgrade recorded: {} upgraded at slot {} (tx {}) -- the \
                         running decoder was generated from the pre-upgrade IDL; see \
                         RUNBOOK.md 'After a program upgrade'",
                        up.program,
                        up.upgrade_slot,
                        up.signature,
                    );
                }
                log::debug!(
                    "flushed {} write ops in {:?} (max slot {max_slot})",
                    buf.len(),
                    elapsed,
                );
                buf.clear();
                return false;
            }
            Err(e) => {
                log::error!(
                    "batch flush of {} ops FAILED, retrying in {:?}: {e}",
                    buf.len(),
                    backoff
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancellation.cancelled() => {
                        // This buffer never committed. Every write here IS idempotent and
                        // WOULD be safely re-derived by the next run -- but only if nothing
                        // downstream already told the world these rows exist. That is exactly
                        // what `FlushOutcome::OpsDropped` (returned by the flusher this call is
                        // part of) is for: it forces every completion-marker call site
                        // (backfill_complete, a cursor clear, snapshot_slot,
                        // last_contiguous_slot) to skip that write instead of laundering this
                        // failure into a false claim of completeness. See the Task-4 fix-round
                        // report, Finding 2.
                        log::error!(
                            "cancelled while retrying a failed flush; DROPPING {} un-committed \
                             write ops (slots up to {max_slot}); reporting FlushOutcome::OpsDropped \
                             so the caller does not write a completion marker for them",
                            buf.len()
                        );
                        buf.clear();
                        return true;
                    }
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// One transaction, in phase order (see the module docs): the backfill cursor is the last
/// statement, so it can never become visible ahead of the rows it vouches for -- if the commit
/// fails, it rolls back with them.
///
/// `ops` is borrowed (and each row cloned into the query) rather than consumed, because the
/// caller has to be able to retry the identical batch after a failed commit.
///
/// Returns the upgrade boundaries this batch actually inserted (not the ones deduplicated by
/// `ON CONFLICT DO NOTHING`), valid only once the commit inside has succeeded -- which it has,
/// whenever this returns `Ok`.
async fn commit_batch(pool: &PgPool, ops: &[WriteOp]) -> Result<Vec<NewUpgrade>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let mut new_upgrades = Vec::new();

    for op in ops {
        match op {
            WriteOp::UpsertConfig(row) => {
                db::accounts::upsert_config(&mut *tx, row.clone()).await?;
            }
            WriteOp::UpsertAdmin(row) => {
                db::accounts::upsert_admin(&mut *tx, row.clone()).await?;
            }
            WriteOp::UpsertRoleAccount(row) => {
                db::accounts::upsert_role_account(&mut *tx, row.clone()).await?;
            }
            WriteOp::UpsertRegionsAccount(row) => {
                db::regions::upsert(&mut *tx, row).await?;
            }
            WriteOp::UpsertMarketplaceAccount(row) => {
                db::marketplace::upsert(&mut *tx, row).await?;
            }
            WriteOp::UpsertPropertyAccount(row) => {
                db::property::upsert(&mut *tx, row).await?;
            }
            WriteOp::UpsertRealxhubAccount(row) => {
                db::realxhub::upsert(&mut *tx, row).await?;
            }
            WriteOp::InsertInstruction(row) => {
                db::instructions::insert_instruction(&mut *tx, row.clone()).await?;
            }
            WriteOp::InsertAction(row) => {
                db::actions::insert_action(&mut *tx, row.clone()).await?;
            }
            WriteOp::CloseAccount {
                table,
                pubkey,
                slot,
            } => {
                db::close::close_in_table(&mut *tx, *table, pubkey, *slot).await?;
            }
            WriteOp::CloseLettingAgentIfLast {
                pubkey,
                removed_postcode,
                slot,
            } => {
                let postcode = serde_json::Value::String(removed_postcode.clone());
                db::property::close_letting_agent_if_last(&mut *tx, pubkey, &postcode, *slot)
                    .await?;
            }
            WriteOp::CloseShareListingIfEmptied {
                pubkey,
                bought_amount,
                slot,
            } => {
                db::marketplace::close_share_listing_if_emptied(
                    &mut *tx,
                    pubkey,
                    *bought_amount,
                    *slot,
                )
                .await?;
            }
            WriteOp::CloseShareListingIfEmptiedByOffer {
                pubkey,
                offer_pubkey,
                slot,
            } => {
                db::marketplace::close_share_listing_if_emptied_by_offer(
                    &mut *tx,
                    pubkey,
                    offer_pubkey,
                    *slot,
                )
                .await?;
            }
            WriteOp::CloseRealxhubShareListingIfEmptied {
                pubkey,
                bought_amount,
                slot,
            } => {
                db::realxhub::close_share_listing_if_emptied(
                    &mut *tx,
                    pubkey,
                    *bought_amount,
                    *slot,
                )
                .await?;
            }
            WriteOp::CloseUnknownAccount { pubkey, slot } => {
                for table in StateTable::ALL {
                    db::close::close_in_table(&mut *tx, *table, pubkey, *slot).await?;
                }
            }
            WriteOp::SetBackfillCursor {
                program_id,
                signature,
                slot,
            } => {
                db::backfill_cursor::set_cursor(&mut *tx, program_id, signature, *slot).await?;
            }
            WriteOp::RecordProgramUpgrade {
                program,
                program_id,
                upgrade_slot,
                signature,
            } => {
                let inserted =
                    db::upgrades::record_upgrade(&mut *tx, program_id, *upgrade_slot, signature)
                        .await?;
                if inserted {
                    new_upgrades.push(NewUpgrade {
                        program,
                        upgrade_slot: *upgrade_slot,
                        signature: signature.clone(),
                    });
                }
            }
            WriteOp::RecordWebhookEvent {
                event_id,
                event_type,
                payload,
                slot,
                tx_signature,
                block_time,
            } => {
                // `ON CONFLICT (event_id) DO NOTHING`: a re-walked `init_property_assets` is a
                // no-op, so the delivery queue never double-records an asset.
                db::webhooks::record_event(
                    &mut *tx,
                    event_id,
                    event_type,
                    payload,
                    *slot,
                    tx_signature,
                    *block_time,
                )
                .await?;
            }
        }
    }

    tx.commit().await?;
    Ok(new_upgrades)
}

#[cfg(test)]
mod tests {
    use super::WriteOp;
    use crate::db::models::{ActionType, NewAction};
    use chrono::Utc;
    use std::str::FromStr;
    use tokio_util::sync::CancellationToken;

    fn action(slot: i64) -> WriteOp {
        WriteOp::InsertAction(NewAction {
            id: format!("sig-{slot}"),
            action_type: ActionType::AdminAdded,
            subject: None,
            role: None,
            permission: None,
            actor: "a".into(),
            slot,
            block_time: Utc::now(),
            tx_signature: "sig".into(),
            instruction_index: "0".into(),
        })
    }

    #[test]
    fn ops_sort_into_the_documented_phase_order() {
        let mut ops = [
            // Deliberately the reverse of the documented order: the backfill cursor (which
            // claims "everything above this is committed") is pushed first here, and must still
            // end up committed last.
            WriteOp::SetBackfillCursor {
                program_id: vec![9],
                signature: "sig".into(),
                slot: 1,
            },
            WriteOp::CloseAccount {
                table: crate::db::close::StateTable::Admin,
                pubkey: vec![1],
                slot: 1,
            },
            action(1),
            WriteOp::UpsertAdmin(crate::db::models::AdminAccount {
                pubkey: vec![1],
                slot: 1,
                lamports: 0,
                admin: vec![2],
                bump: 1,
            }),
            WriteOp::InsertInstruction(crate::db::models::NewInstruction {
                program_id: vec![9],
                signature: vec![0],
                ix_index: 0,
                inner_index: -1,
                slot: 1,
                block_time: Utc::now(),
                ix_name: "add_admin".into(),
                accounts: vec![],
                data: serde_json::json!({}),
            }),
            WriteOp::RecordProgramUpgrade {
                program: "marketplace",
                program_id: vec![9],
                upgrade_slot: 1,
                signature: "sig".into(),
            },
        ];
        ops.sort_by_key(WriteOp::phase);
        let phases: Vec<u8> = ops.iter().map(WriteOp::phase).collect();
        assert_eq!(phases, vec![0, 0, 1, 2, 3, 4]);
    }

    #[test]
    fn a_batch_reports_its_highest_slot() {
        let ops = [action(10), action(30), action(20)];
        assert_eq!(ops.iter().map(WriteOp::slot).max(), Some(30));
    }

    // --- FINDING 2 (Task-4 fix round): the flusher must report a dropped batch, not just log it
    #[tokio::test]
    async fn a_persistently_failing_commit_that_is_cancelled_reports_dropped_ops() {
        // A pool pointed at a database that cannot exist, on the same Postgres server the rest
        // of this suite already requires (see env-notes.md): every commit attempt fails
        // immediately and deterministically, no live outage or timing-sensitive setup needed to
        // exercise `flush`'s cancellation-drop path.
        let base = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set (see env-notes.md) to run this test");
        let options = sqlx::postgres::PgConnectOptions::from_str(&base)
            .expect("DATABASE_URL must be a valid postgres URL")
            .database("indexer_test_db_that_must_not_exist_9f3c2a");
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy_with(options);

        let cancellation = CancellationToken::new();
        // Already cancelled: the first failed commit's `tokio::select!` between the backoff
        // sleep and `cancellation.cancelled()` resolves to the cancellation branch immediately.
        cancellation.cancel();

        let mut buf = vec![WriteOp::SetBackfillCursor {
            program_id: vec![9],
            signature: "sig".into(),
            slot: 1,
        }];

        let dropped = super::flush(&pool, &mut buf, &cancellation).await;
        assert!(
            dropped,
            "a commit that can never succeed, cancelled during its retry backoff, must report \
             dropped ops"
        );
        assert!(buf.is_empty(), "the dropped batch must still be cleared");
    }
}
