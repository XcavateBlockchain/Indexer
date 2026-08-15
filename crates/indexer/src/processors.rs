//! The three carbon processors: instructions, account updates, account deletions.
//!
//! All three are thin. They decode-adjacent work only: convert carbon's types into
//! [`crate::batcher::WriteOp`]s and hand them to the batcher, then return. No processor opens
//! a database transaction, because holding one would stall carbon's single update loop for the
//! length of a round trip.
//!
//! One deliberate exception to "thin": the instruction processor may `await` a `getBlockTime`
//! RPC call on a cache miss. Ruling R14 requires it -- `block_time` is `NOT NULL` in both
//! tables and the Yellowstone transaction stream does not carry it, so the alternatives are
//! blocking briefly or writing a guessed timestamp. We block.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use carbon_core::account::{AccountProcessorInputType, DecodedAccount};
use carbon_core::datasource::AccountDeletion;
use carbon_core::error::{CarbonResult, Error as CarbonError};
use carbon_core::instruction::InstructionProcessorInputType;
use carbon_core::metrics::MetricsCollection;
use carbon_core::processor::Processor;
use carbon_xcavate_whitelist_decoder::accounts::XcavateWhitelistAccount;
use carbon_xcavate_whitelist_decoder::instructions::XcavateWhitelistInstruction;
use solana_pubkey::Pubkey;
use tokio::sync::RwLock;

use crate::batcher::{Batcher, WriteOp};
use crate::block_time::BlockTimeResolver;
use crate::db::models::{AdminAccount, ConfigAccount, RoleAccountRow};
use crate::mapping::{self, PendingClose};

/// The set of PDAs the Yellowstone datasource should watch for deletion.
///
/// Carbon only emits `AccountDeletion` for pubkeys present in this set (the datasource checks
/// it before synthesising the event), so it has to be fed: seeded at startup from every
/// still-open state row, and extended with every account update we see.
pub type TrackedAccounts = Arc<RwLock<HashSet<Pubkey>>>;

// --- instructions ---------------------------------------------------------------------------

pub struct InstructionProcessor {
    batcher: Batcher,
    block_time: Arc<BlockTimeResolver>,
}

impl InstructionProcessor {
    pub fn new(batcher: Batcher, block_time: Arc<BlockTimeResolver>) -> Self {
        Self {
            batcher,
            block_time,
        }
    }
}

#[async_trait]
impl Processor for InstructionProcessor {
    type InputType = InstructionProcessorInputType<XcavateWhitelistInstruction>;

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

        let mapped = match mapping::map_instruction(&metadata, &decoded, block_time) {
            Ok(Some(mapped)) => mapped,
            // The decoder's synthetic `CpiEvent` variant. This program emits log events, not
            // CPI events, so this is unreachable in practice -- but it is a legitimate
            // "nothing to write", not a failure.
            Ok(None) => return Ok(()),
            Err(e) => {
                // An instruction that decoded against this program's IDL but does not fit the
                // mapping contract means the deployed program and the checked-in IDL have
                // diverged. The old SubQuery handler threw on exactly this condition ("data
                // integrity beats liveness"); the equivalent here is failing the update, which
                // logs, bumps carbon's `updates_failed`, and skips the rest of this
                // transaction rather than writing a half-mapped history.
                crate::metrics::inc_decode_skipped(e.reason());
                log::error!(
                    "unmappable whitelist instruction in tx {} at path {}: {e}",
                    tx.signature,
                    mapping::instruction_index(&metadata.absolute_path),
                );
                return Err(CarbonError::Custom(e.to_string()));
            }
        };

        let mut ops = Vec::with_capacity(3);
        ops.push(WriteOp::InsertInstruction(mapped.instruction));
        ops.push(WriteOp::InsertAction(mapped.action));
        // Ruling R11: an instruction that closes a PDA has to soft-close the state row itself.
        // The account stream cannot do it -- a closed account stops matching the owner filter,
        // so the last thing we would ever see for that pubkey is its pre-close state.
        if let Some(close) = mapped.close {
            ops.push(match close {
                PendingClose::Admin { pubkey, slot } => WriteOp::CloseAdmin { pubkey, slot },
                PendingClose::RoleAccount { pubkey, slot } => {
                    WriteOp::CloseRoleAccount { pubkey, slot }
                }
            });
        }

        self.batcher
            .push_many(ops)
            .await
            .map_err(|e| CarbonError::Custom(format!("batcher channel closed: {e}")))
    }
}

// --- account updates ------------------------------------------------------------------------

pub struct AccountProcessor {
    batcher: Batcher,
    tracked: TrackedAccounts,
}

impl AccountProcessor {
    pub fn new(batcher: Batcher, tracked: TrackedAccounts) -> Self {
        Self { batcher, tracked }
    }
}

#[async_trait]
impl Processor for AccountProcessor {
    type InputType = AccountProcessorInputType<XcavateWhitelistAccount>;

    async fn process(
        &mut self,
        (meta, decoded, raw): Self::InputType,
        _metrics: Arc<MetricsCollection>,
    ) -> CarbonResult<()> {
        // Every PDA we see becomes deletion-tracked, so a later close reaches the deletion
        // pipe instead of being dropped by the datasource.
        self.tracked.write().await.insert(meta.pubkey);

        let op = account_write_op(meta.pubkey, meta.slot as i64, raw.lamports as i64, &decoded);

        self.batcher
            .push(op)
            .await
            .map_err(|e| CarbonError::Custom(format!("batcher channel closed: {e}")))
    }
}

/// Decoded account -> state-table upsert. `lamports` comes from the raw account (the decoded
/// wrapper carries it too, but the raw account is the authoritative copy carbon received).
///
/// Shared with the `getProgramAccounts` snapshot loader ([`crate::snapshot`]), which decodes
/// with the same decoder and then calls this: the snapshot must produce byte-identical rows to
/// the live account stream, and the only way to guarantee that is to run the same mapping.
///
/// `closed_at_slot` is not a field here on purpose: Task 2's upserts hardcode `NULL` for it in
/// the `VALUES` list and include it in the `DO UPDATE SET` column list, so any live update at a
/// newer slot revives a soft-closed row. That is the correct behaviour for a PDA that is
/// closed and later re-created at the same address.
pub(crate) fn account_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    decoded: &DecodedAccount<XcavateWhitelistAccount>,
) -> WriteOp {
    let pubkey = pubkey.to_bytes().to_vec();
    match &decoded.data {
        XcavateWhitelistAccount::Config(config) => WriteOp::UpsertConfig(ConfigAccount {
            pubkey,
            slot,
            lamports,
            authority: config.authority.to_bytes().to_vec(),
            pending_authority: config.pending_authority.map(|p| p.to_bytes().to_vec()),
            bump: config.bump as i16,
        }),
        XcavateWhitelistAccount::Admin(admin) => WriteOp::UpsertAdmin(AdminAccount {
            pubkey,
            slot,
            lamports,
            admin: admin.admin.to_bytes().to_vec(),
            bump: admin.bump as i16,
        }),
        XcavateWhitelistAccount::RoleAccount(role) => WriteOp::UpsertRoleAccount(RoleAccountRow {
            pubkey,
            slot,
            lamports,
            user_pubkey: role.user.to_bytes().to_vec(),
            role: mapping::role_from_chain(&role.role),
            permission: mapping::permission_from_chain(&role.permission),
            rent_payer: role.rent_payer.to_bytes().to_vec(),
            bump: role.bump as i16,
        }),
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
