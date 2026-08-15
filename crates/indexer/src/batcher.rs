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
//! sync_state) so that a close always lands after the upsert it is closing, even when both
//! arrive in the same batch. Every individual write is idempotent and slot-guarded by Task 2's
//! db module, so ordering *between* batches never matters -- this ordering only removes the
//! within-batch race.

use std::sync::Arc;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::db;
use crate::db::models::{AdminAccount, ConfigAccount, NewAction, NewInstruction, RoleAccountRow};
use crate::sync_frontier::SyncFrontier;

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
    InsertInstruction(NewInstruction),
    InsertAction(NewAction),
    /// Instruction-driven close of an `admin` PDA (ruling R11).
    CloseAdmin {
        pubkey: Vec<u8>,
        slot: i64,
    },
    /// Instruction-driven close of a `role_account` PDA (ruling R11).
    CloseRoleAccount {
        pubkey: Vec<u8>,
        slot: i64,
    },
    /// Close driven by carbon's `AccountDeletion`, which carries only `{pubkey, slot}` and so
    /// cannot say which table the pubkey belongs to. Tries all three; each is a guarded
    /// `UPDATE ... WHERE pubkey = $1 AND slot < $2`, so the two that do not match are no-ops.
    CloseUnknownAccount {
        pubkey: Vec<u8>,
        slot: i64,
    },
}

impl WriteOp {
    /// The slot this write is evidence for. The flusher takes the max across a batch as the
    /// candidate `last_contiguous_slot`.
    pub fn slot(&self) -> i64 {
        match self {
            WriteOp::UpsertConfig(r) => r.slot,
            WriteOp::UpsertAdmin(r) => r.slot,
            WriteOp::UpsertRoleAccount(r) => r.slot,
            WriteOp::InsertInstruction(r) => r.slot,
            WriteOp::InsertAction(r) => r.slot,
            WriteOp::CloseAdmin { slot, .. }
            | WriteOp::CloseRoleAccount { slot, .. }
            | WriteOp::CloseUnknownAccount { slot, .. } => *slot,
        }
    }

    /// Position of this op's kind in the within-transaction ordering (see module docs).
    fn phase(&self) -> u8 {
        match self {
            WriteOp::UpsertConfig(_) | WriteOp::UpsertAdmin(_) | WriteOp::UpsertRoleAccount(_) => 0,
            WriteOp::InsertInstruction(_) => 1,
            WriteOp::InsertAction(_) => 2,
            WriteOp::CloseAdmin { .. }
            | WriteOp::CloseRoleAccount { .. }
            | WriteOp::CloseUnknownAccount { .. } => 3,
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
    pub async fn push(&self, op: WriteOp) -> Result<(), mpsc::error::SendError<WriteOp>> {
        self.tx.send(op).await
    }

    pub async fn push_many(
        &self,
        ops: impl IntoIterator<Item = WriteOp>,
    ) -> Result<(), mpsc::error::SendError<WriteOp>> {
        for op in ops {
            self.tx.send(op).await?;
        }
        Ok(())
    }
}

/// Creates the channel and spawns the flusher.
///
/// The returned [`JoinHandle`] completes once every [`Batcher`] clone has been dropped *and*
/// the final partial batch has been committed -- so a graceful shutdown is: drop the pipeline
/// (which drops the processors, which drop their `Batcher`s), then await this handle.
pub fn spawn(
    pool: PgPool,
    frontier: Arc<SyncFrontier>,
    cancellation: CancellationToken,
) -> (Batcher, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let handle = tokio::spawn(flusher_loop(pool, rx, frontier, cancellation));
    (Batcher { tx }, handle)
}

async fn flusher_loop(
    pool: PgPool,
    mut rx: mpsc::Receiver<WriteOp>,
    frontier: Arc<SyncFrontier>,
    cancellation: CancellationToken,
) {
    let mut buf: Vec<WriteOp> = Vec::with_capacity(MAX_BATCH);
    // `None` while the buffer is empty: an idle flusher must not wake up every 250 ms.
    let mut deadline: Option<tokio::time::Instant> = None;

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
                        flush(&pool, &mut buf, &frontier, &cancellation).await;
                        deadline = None;
                    }
                }
                None => {
                    // All senders dropped: commit whatever is left and finish.
                    flush(&pool, &mut buf, &frontier, &cancellation).await;
                    log::info!("batch flusher stopped (all writers dropped)");
                    return;
                }
            },

            _ = timer => {
                flush(&pool, &mut buf, &frontier, &cancellation).await;
                deadline = None;
            }
        }
    }
}

/// Commits `buf` in one transaction, retrying with exponential backoff until it succeeds or
/// the process is cancelled. `buf` is left empty on success.
async fn flush(
    pool: &PgPool,
    buf: &mut Vec<WriteOp>,
    frontier: &SyncFrontier,
    cancellation: &CancellationToken,
) {
    if buf.is_empty() {
        return;
    }

    // Stable sort by phase: within a phase the original arrival order is preserved, which
    // keeps the ordering of two writes to the same row deterministic.
    buf.sort_by_key(WriteOp::phase);

    let max_slot = buf.iter().map(WriteOp::slot).max().unwrap_or(0);
    let advance_to = if max_slot >= 0 && frontier.may_advance(max_slot as u64) {
        Some(max_slot)
    } else {
        None
    };

    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        match commit_batch(pool, buf, advance_to).await {
            Ok(()) => {
                let elapsed = started.elapsed();
                crate::metrics::record_flush(elapsed, buf.len());
                log::debug!(
                    "flushed {} write ops in {:?} (max slot {max_slot}{})",
                    buf.len(),
                    elapsed,
                    match advance_to {
                        Some(s) => format!(", advanced last_contiguous_slot to {s}"),
                        None => String::new(),
                    }
                );
                buf.clear();
                return;
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
                        log::error!(
                            "cancelled while retrying a failed flush; DROPPING {} un-committed \
                             write ops (slots up to {max_slot}). They will be re-derived on the \
                             next run: every write here is idempotent and the sync frontier was \
                             not advanced.",
                            buf.len()
                        );
                        buf.clear();
                        return;
                    }
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// One transaction. `advance_to` is applied last, inside the same transaction, so the sync
/// frontier can never be visible ahead of the rows it claims are contiguous -- if the commit
/// fails, the advance rolls back with it.
///
/// `ops` is borrowed (and each row cloned into the query) rather than consumed, because the
/// caller has to be able to retry the identical batch after a failed commit.
async fn commit_batch(
    pool: &PgPool,
    ops: &[WriteOp],
    advance_to: Option<i64>,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

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
            WriteOp::InsertInstruction(row) => {
                db::instructions::insert_instruction(&mut *tx, row.clone()).await?;
            }
            WriteOp::InsertAction(row) => {
                db::actions::insert_action(&mut *tx, row.clone()).await?;
            }
            WriteOp::CloseAdmin { pubkey, slot } => {
                db::accounts::close_admin(&mut *tx, pubkey, *slot).await?;
            }
            WriteOp::CloseRoleAccount { pubkey, slot } => {
                db::accounts::close_role_account(&mut *tx, pubkey, *slot).await?;
            }
            WriteOp::CloseUnknownAccount { pubkey, slot } => {
                db::accounts::close_config(&mut *tx, pubkey, *slot).await?;
                db::accounts::close_admin(&mut *tx, pubkey, *slot).await?;
                db::accounts::close_role_account(&mut *tx, pubkey, *slot).await?;
            }
        }
    }

    if let Some(slot) = advance_to {
        db::sync_state::advance_last_contiguous_slot(&mut *tx, slot).await?;
    }

    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::WriteOp;
    use crate::db::models::{ActionType, NewAction};
    use chrono::Utc;

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
            WriteOp::CloseAdmin {
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
                signature: vec![0],
                ix_index: 0,
                inner_index: -1,
                slot: 1,
                block_time: Utc::now(),
                ix_name: "add_admin".into(),
                accounts: vec![],
                data: serde_json::json!({}),
            }),
        ];
        ops.sort_by_key(WriteOp::phase);
        let phases: Vec<u8> = ops.iter().map(WriteOp::phase).collect();
        assert_eq!(phases, vec![0, 1, 2, 3]);
    }

    #[test]
    fn batch_max_slot_is_the_frontier_candidate() {
        let ops = [action(10), action(30), action(20)];
        assert_eq!(ops.iter().map(WriteOp::slot).max(), Some(30));
    }
}
