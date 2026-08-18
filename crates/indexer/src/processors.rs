//! The carbon processors: instructions, account updates, account deletions.
//!
//! All are thin. They do decode-adjacent work only: convert carbon's types into
//! [`crate::batcher::WriteOp`]s and hand them to the batcher, then return. No processor opens
//! a database transaction, because holding one would stall carbon's single update loop for the
//! length of a round trip.
//!
//! The instruction and account processors are generic over a [`ProgramMapper`] -- one typed
//! instantiation per program is registered on the pipeline (`pipeline::common_pipes`), each
//! paired with that program's decoder. The deletion processor is shared: an `AccountDeletion`
//! carries only `{pubkey, slot}` and pubkeys are globally unique, so one processor (and one
//! tracked-accounts set) serves every program.
//!
//! One deliberate exception to "thin": the instruction processor may `await` a `getBlockTime`
//! RPC call on a cache miss. Ruling R14 requires it -- `block_time` is `NOT NULL` in both
//! history tables and the Yellowstone transaction stream does not carry it, so the
//! alternatives are blocking briefly or writing a guessed timestamp. We block.

use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::Arc;

use async_trait::async_trait;
use carbon_core::account::AccountProcessorInputType;
use carbon_core::datasource::AccountDeletion;
use carbon_core::error::{CarbonResult, Error as CarbonError};
use carbon_core::instruction::InstructionProcessorInputType;
use carbon_core::metrics::MetricsCollection;
use carbon_core::processor::Processor;
use solana_pubkey::Pubkey;
use tokio::sync::RwLock;

use crate::batcher::{Batcher, WriteOp};
use crate::block_time::BlockTimeResolver;
use crate::mapping::{self, PendingClose, ProgramMapper};

/// The set of PDAs the Yellowstone datasource should watch for deletion.
///
/// Carbon only emits `AccountDeletion` for pubkeys present in this set (the datasource checks
/// it before synthesising the event), so it has to be fed: seeded at startup from every
/// still-open state row across every program's tables, and extended with every account update
/// we see. One shared set serves all programs -- pubkeys are globally unique.
pub type TrackedAccounts = Arc<RwLock<HashSet<Pubkey>>>;

// --- instructions ---------------------------------------------------------------------------

pub struct InstructionProcessor<M: ProgramMapper> {
    batcher: Batcher,
    block_time: Arc<BlockTimeResolver>,
    _mapper: PhantomData<M>,
}

impl<M: ProgramMapper> InstructionProcessor<M> {
    pub fn new(batcher: Batcher, block_time: Arc<BlockTimeResolver>) -> Self {
        Self {
            batcher,
            block_time,
            _mapper: PhantomData,
        }
    }
}

#[async_trait]
impl<M: ProgramMapper> Processor for InstructionProcessor<M> {
    type InputType = InstructionProcessorInputType<M::Ix>;

    async fn process(
        &mut self,
        (metadata, decoded, _nested, _raw): Self::InputType,
        _metrics: Arc<MetricsCollection>,
    ) -> CarbonResult<()> {
        let tx = metadata.transaction_metadata.clone();
        let block_time = self
            .block_time
            .resolve(tx.slot, tx.block_time)
            .await
            .map_err(|e| CarbonError::Custom(e.to_string()))?;

        let mapped = match M::map_instruction(&metadata, &decoded, block_time) {
            Ok(Some(mapped)) => mapped,
            // The decoder's synthetic `CpiEvent` variant. Every program in this protocol
            // emits log events, not CPI events, so this is unreachable in practice -- but it
            // is a legitimate "nothing to write", not a failure.
            Ok(None) => return Ok(()),
            Err(e) => {
                // An instruction that decoded against this program's IDL but does not fit the
                // mapping contract means the deployed program and the checked-in IDL have
                // diverged. The old SubQuery handler threw on exactly this condition ("data
                // integrity beats liveness"); the equivalent here is failing the update, which
                // logs, bumps carbon's `updates_failed`, and skips the rest of this
                // transaction rather than writing a half-mapped history.
                crate::metrics::inc_decode_skipped(M::NAME, e.reason());
                log::error!(
                    "unmappable {} instruction in tx {} at path {}: {e}",
                    M::NAME,
                    tx.signature,
                    mapping::instruction_index(&metadata.absolute_path),
                );
                return Err(CarbonError::Custom(e.to_string()));
            }
        };

        let mut ops = Vec::with_capacity(2 + mapped.closes.len());
        ops.push(WriteOp::InsertInstruction(mapped.instruction));
        if let Some(action) = mapped.action {
            ops.push(WriteOp::InsertAction(action));
        }
        // Ruling R11: an instruction that closes a PDA has to soft-close the state row itself.
        // The account stream cannot do it -- a closed account stops matching the owner filter,
        // so the last thing we would ever see for that pubkey is its pre-close state.
        for close in mapped.closes {
            ops.push(match close {
                PendingClose::Account {
                    table,
                    pubkey,
                    slot,
                } => WriteOp::CloseAccount {
                    table,
                    pubkey,
                    slot,
                },
                PendingClose::LettingAgentIfLast {
                    pubkey,
                    removed_postcode,
                    slot,
                } => WriteOp::CloseLettingAgentIfLast {
                    pubkey,
                    removed_postcode,
                    slot,
                },
            });
        }

        self.batcher
            .push_many(ops)
            .await
            .map_err(|e| CarbonError::Custom(format!("batcher channel closed: {e}")))
    }
}

// --- account updates ------------------------------------------------------------------------

pub struct AccountProcessor<M: ProgramMapper> {
    batcher: Batcher,
    tracked: TrackedAccounts,
    _mapper: PhantomData<M>,
}

impl<M: ProgramMapper> AccountProcessor<M> {
    pub fn new(batcher: Batcher, tracked: TrackedAccounts) -> Self {
        Self {
            batcher,
            tracked,
            _mapper: PhantomData,
        }
    }
}

#[async_trait]
impl<M: ProgramMapper> Processor for AccountProcessor<M> {
    type InputType = AccountProcessorInputType<M::Acc>;

    async fn process(
        &mut self,
        (meta, decoded, raw): Self::InputType,
        _metrics: Arc<MetricsCollection>,
    ) -> CarbonResult<()> {
        // Every PDA we see becomes deletion-tracked, so a later close reaches the deletion
        // pipe instead of being dropped by the datasource.
        self.tracked.write().await.insert(meta.pubkey);

        // `lamports` comes from the raw account (the decoded wrapper carries it too, but the
        // raw account is the authoritative copy carbon received).
        let op = M::account_write_op(meta.pubkey, meta.slot as i64, raw.lamports as i64, &decoded);

        self.batcher
            .push(op)
            .await
            .map_err(|e| CarbonError::Custom(format!("batcher channel closed: {e}")))
    }
}

// --- account deletions ----------------------------------------------------------------------

/// The redundant safety net (per the architecture ruling): the instruction-driven close is
/// primary, because a closed account stops matching the owner filter and so may never produce
/// a deletion event at all. This pipe exists for the cases the instruction path cannot cover
/// -- e.g. an account closed by a program upgrade or a future instruction we do not map.
pub struct AccountDeletionProcessor {
    batcher: Batcher,
}

impl AccountDeletionProcessor {
    pub fn new(batcher: Batcher) -> Self {
        Self { batcher }
    }
}

#[async_trait]
impl Processor for AccountDeletionProcessor {
    type InputType = AccountDeletion;

    async fn process(
        &mut self,
        deletion: AccountDeletion,
        _metrics: Arc<MetricsCollection>,
    ) -> CarbonResult<()> {
        log::info!(
            "account deletion for {} at slot {} (tx {:?})",
            deletion.pubkey,
            deletion.slot,
            deletion.transaction_signature
        );
        self.batcher
            .push(WriteOp::CloseUnknownAccount {
                pubkey: deletion.pubkey.to_bytes().to_vec(),
                slot: deletion.slot as i64,
            })
            .await
            .map_err(|e| CarbonError::Custom(format!("batcher channel closed: {e}")))
    }
}
