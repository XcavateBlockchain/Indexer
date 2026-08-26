//! Pure decoded-instruction -> database-rows mappings, one submodule per program.
//!
//! Deliberately pure and side-effect free (no DB, no clock, no RPC) so the whole mapping
//! contract is unit-testable without a database or a chain. This module holds what is shared
//! -- the error type, the instruction-row builder, the [`ProgramMapper`] trait the generic
//! processors are instantiated over -- and each program's submodule holds its own contract:
//!
//! * [`whitelist`] -- the port of the old SubQuery `mappingHandlers.ts` (ruling R7: one
//!   instruction => one `program_instructions` row + one `whitelist_actions` row).
//! * [`regions`], [`marketplace`], [`property`] -- the sibling programs. One instruction =>
//!   one `program_instructions` row; there is no per-program action log for them (the
//!   whitelist's exists for SubQuery parity only), so current state comes from the
//!   account-state tables and history from `program_instructions`.
//!
//! Every mapping also emits [`PendingClose`]s for instructions that close PDAs (ruling R11):
//! a closed account stops matching the owner-scoped account filter, so the account stream
//! never delivers its final state -- the instruction is the only reliable evidence. The close
//! positions are per-instruction facts read from each program's on-chain source (`close =`
//! constraints), NOT per-account-type constants: the same account type can be closed at
//! different positions by different instructions.

use carbon_core::instruction::InstructionMetadata;
use chrono::{DateTime, Utc};
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;

use crate::db::close::StateTable;
use crate::db::models::{NewAction, NewInstruction};

pub mod marketplace;
pub mod property;
pub mod regions;
pub mod whitelist;

/// What one decoded instruction turns into.
#[derive(Debug, Clone)]
pub struct MappedInstruction {
    pub instruction: NewInstruction,
    /// The whitelist parity log row (`whitelist_actions`). `None` for the sibling programs,
    /// which have no action log.
    pub action: Option<NewAction>,
    /// Slot-guarded soft closes implied by this instruction (ruling R11). Usually empty or
    /// one entry; marketplace has instructions that close two PDAs at once.
    pub closes: Vec<PendingClose>,
    /// Outbound webhook events this instruction records (ADR-28). Usually empty; only
    /// marketplace's `init_property_assets` emits one (a new property asset registered).
    /// The batcher commits each as a durable `webhook_events` row
    /// (`WriteOp::RecordWebhookEvent`) and a background loop delivers it.
    pub webhook_events: Vec<WebhookEvent>,
}

/// One durable, idempotent webhook notification the mapper wants recorded (ADR-28).
///
/// Carries only the on-chain evidence: the delivery loop (which owns the clock and the
/// network) POSTs [`WebhookEvent::payload`] to `WEBHOOK_URL` and stamps the delivery
/// timestamps in `webhook_events`. `event_id` is the `ON CONFLICT` key, so a backfill
/// re-walk re-delivering the same instruction is a no-op and the notification fires at most
/// once.
#[derive(Debug, Clone)]
pub struct WebhookEvent {
    /// `<event_type>:<base58 subject key>` -- the dedup key (`webhook_events` primary key).
    pub event_id: String,
    /// Low-cardinality event label (`property_asset_registered`).
    pub event_type: &'static str,
    /// The JSON document the delivery loop POSTs to `WEBHOOK_URL`.
    pub payload: serde_json::Value,
    /// Slot of the transaction that produced this event (provenance).
    pub slot: i64,
    /// base58 signature of that transaction.
    pub tx_signature: String,
    /// The transaction's block time.
    pub block_time: DateTime<Utc>,
}

/// A soft close implied by an instruction. The pubkey is taken from the instruction's own
/// account list rather than re-derived from PDA seeds -- the account being closed is always
/// present in the instruction, so there is nothing to derive and nothing to get wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingClose {
    /// The common case: this instruction unconditionally closed the PDA at some account-list
    /// position, and `table` is the state table its row lives in.
    Account {
        table: StateTable,
        pubkey: Vec<u8>,
        slot: i64,
    },
    /// A conditional close the mapper cannot decide (a runtime `close()` call behind a
    /// condition on account state, not an Anchor constraint): property's
    /// `remove_letting_agent` closes the `LettingAgent` PDA only when the removed location
    /// was its last. The decision lives in the batcher's write
    /// (`db::property::close_letting_agent_if_last`); this op just carries the evidence.
    /// `removed_postcode` is the postcode arg as a UTF-8 string, matching the shape the
    /// `locations` JSONB stores.
    LettingAgentIfLast {
        pubkey: Vec<u8>,
        removed_postcode: String,
        slot: i64,
    },
    /// Conditional close of a `ShareListing` by `buy_relisted_shares`, which on-chain closes
    /// the PDA only when the buy emptied it. The instruction's `amount` arg carries how many
    /// shares were bought; the batcher's write
    /// (`db::marketplace::close_share_listing_if_emptied`) compares it against the stored
    /// row's remaining amount.
    ShareListingIfEmptied {
        pubkey: Vec<u8>,
        bought_amount: i64,
        slot: i64,
    },
    /// Conditional close of a `ShareListing` by `accept_offer`, whose sold amount is the
    /// OFFER's amount -- an account fact, not an instruction arg -- so this op carries the
    /// offer's pubkey instead and the batcher's write
    /// (`db::marketplace::close_share_listing_if_emptied_by_offer`) reads the amount from
    /// the stored offer row.
    ShareListingIfEmptiedByOffer {
        pubkey: Vec<u8>,
        offer_pubkey: Vec<u8>,
        slot: i64,
    },
}

/// An instruction that decoded but could not be turned into rows.
///
/// This is always a loud error, never a silent drop: the old indexer's stance was that for a
/// compliance registry, data integrity beats liveness, and it carries over to every program.
/// Instructions that fail to *decode* never reach this module -- they show up in carbon's
/// `updates_failed`.
#[derive(Debug)]
pub enum MappingError {
    /// The instruction's account list is shorter than the mapping contract requires. Only
    /// possible if the on-chain program's account order changed without the IDL being
    /// regenerated.
    MissingAccount {
        ix_name: &'static str,
        position: usize,
        available: usize,
    },
    /// carbon always sets `absolute_path` to at least one element; an empty one means the
    /// upstream transformer changed shape.
    EmptyAbsolutePath { ix_name: &'static str },
    /// The decoded args would not serialize to JSON for `program_instructions.data`.
    Serialize {
        ix_name: &'static str,
        source: serde_json::Error,
    },
}

impl MappingError {
    /// Low-cardinality label for `decode_skipped_total`. Never contains a signature or a
    /// pubkey -- those go in the error log line, not in a metric label.
    pub fn reason(&self) -> &'static str {
        match self {
            MappingError::MissingAccount { .. } => "missing_account",
            MappingError::EmptyAbsolutePath { .. } => "empty_absolute_path",
            MappingError::Serialize { .. } => "serialize",
        }
    }
}

impl std::fmt::Display for MappingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MappingError::MissingAccount {
                ix_name,
                position,
                available,
            } => write!(
                f,
                "{ix_name}: account position {position} is out of range ({available} accounts present)"
            ),
            MappingError::EmptyAbsolutePath { ix_name } => {
                write!(f, "{ix_name}: instruction metadata has an empty absolute_path")
            }
            MappingError::Serialize { ix_name, source } => {
                write!(f, "{ix_name}: failed to serialize decoded args to JSON: {source}")
            }
        }
    }
}

impl std::error::Error for MappingError {}

/// The trait the generic processors ([`crate::processors`]) are instantiated over: one impl
/// per program, tying the decoder's typed outputs to that program's row mappings.
pub trait ProgramMapper: Send + Sync + 'static {
    /// The decoder's instruction enum.
    type Ix: Send + Sync;
    /// The decoder's account enum.
    type Acc: Send + Sync;
    /// The registry name (`crate::programs`), used in logs and metric labels.
    const NAME: &'static str;

    fn map_instruction(
        metadata: &InstructionMetadata,
        decoded: &carbon_core::instruction::DecodedInstruction<Self::Ix>,
        block_time: DateTime<Utc>,
    ) -> Result<Option<MappedInstruction>, MappingError>;

    fn account_write_op(
        pubkey: Pubkey,
        slot: i64,
        lamports: i64,
        decoded: &carbon_core::account::DecodedAccount<Self::Acc>,
    ) -> crate::batcher::WriteOp;
}

/// carbon's `absolute_path` -> the old SubQuery `ix.index.join(".")` format: `"3"` for a
/// top-level instruction, `"3.1"` for the second CPI under instruction 3, and so on. This is
/// half of the `whitelist_actions.id`, so the formatting is load-bearing for parity with the
/// old database; the sibling programs reuse it for uniformity.
pub fn instruction_index(absolute_path: &[u8]) -> String {
    absolute_path
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

/// The per-instruction facts every mapping derives from carbon's metadata before doing
/// anything program-specific.
pub(crate) struct IxContext {
    pub slot: i64,
    pub tx_signature: String,
    /// Full dotted path (`"3.1.2"`).
    pub index_str: String,
    pub ix_index: i16,
    /// -1 for a top-level instruction, else the position within the enclosing instruction's
    /// CPI list. Nesting deeper than one level collapses onto the second element of the
    /// path -- acceptable because `index_str` keeps the full path, so no information is lost
    /// overall; only `program_instructions`' composite key is coarser than the path.
    pub inner_index: i16,
}

pub(crate) fn ix_context(
    ix_name: &'static str,
    metadata: &InstructionMetadata,
) -> Result<IxContext, MappingError> {
    let tx = &metadata.transaction_metadata;
    let path = metadata.absolute_path.as_slice();
    if path.is_empty() {
        return Err(MappingError::EmptyAbsolutePath { ix_name });
    }
    Ok(IxContext {
        slot: tx.slot as i64,
        tx_signature: tx.signature.to_string(),
        index_str: instruction_index(path),
        ix_index: path[0] as i16,
        inner_index: if path.len() == 1 { -1 } else { path[1] as i16 },
    })
}

/// Builds the `program_instructions` row every mapping produces: the decoded args serialized
/// to JSON (the generated enums' natural serde output), the account list as raw 32-byte
/// keys, and the program attribution.
pub(crate) fn instruction_row<T: serde::Serialize>(
    program_id: &Pubkey,
    ix_name: &'static str,
    metadata: &InstructionMetadata,
    ctx: &IxContext,
    accounts: &[AccountMeta],
    data: &T,
    block_time: DateTime<Utc>,
) -> Result<NewInstruction, MappingError> {
    Ok(NewInstruction {
        program_id: program_id.to_bytes().to_vec(),
        signature: metadata.transaction_metadata.signature.as_ref().to_vec(),
        ix_index: ctx.ix_index,
        inner_index: ctx.inner_index,
        slot: ctx.slot,
        block_time,
        ix_name: ix_name.to_string(),
        accounts: accounts
            .iter()
            .map(|a| a.pubkey.to_bytes().to_vec())
            .collect(),
        data: serde_json::to_value(data)
            .map_err(|source| MappingError::Serialize { ix_name, source })?,
    })
}

pub(crate) fn account_at(
    accounts: &[AccountMeta],
    position: usize,
    ix_name: &'static str,
) -> Result<String, MappingError> {
    accounts
        .get(position)
        .map(|a| a.pubkey.to_string())
        .ok_or(MappingError::MissingAccount {
            ix_name,
            position,
            available: accounts.len(),
        })
}

pub(crate) fn account_bytes_at(
    accounts: &[AccountMeta],
    position: usize,
    ix_name: &'static str,
) -> Result<Vec<u8>, MappingError> {
    accounts
        .get(position)
        .map(|a| a.pubkey.to_bytes().to_vec())
        .ok_or(MappingError::MissingAccount {
            ix_name,
            position,
            available: accounts.len(),
        })
}

/// Shorthand for the common unconditional close.
pub(crate) fn close_at(
    accounts: &[AccountMeta],
    position: usize,
    ix_name: &'static str,
    table: StateTable,
    slot: i64,
) -> Result<PendingClose, MappingError> {
    Ok(PendingClose::Account {
        table,
        pubkey: account_bytes_at(accounts, position, ix_name)?,
        slot,
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod sibling_tests;
