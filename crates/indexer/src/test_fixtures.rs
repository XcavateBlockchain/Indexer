//! Synthetic carbon fixtures shared by `mapping::tests` and `integration_tests`.
//!
//! Building a `TransactionMetadata` by hand is a little fiddly (it carries a whole
//! `TransactionStatusMeta` and `VersionedMessage`, neither of which the mapping reads), so it
//! lives here once rather than in each test module.

use std::sync::Arc;

use carbon_core::instruction::{DecodedInstruction, InstructionMetadata};
use carbon_core::transaction::TransactionMetadata;
use carbon_xcavate_whitelist_decoder::instructions::XcavateWhitelistInstruction;
use solana_instruction::AccountMeta;
use solana_pubkey::Pubkey;
use solana_signature::Signature;

/// A slot comfortably above the program's deploy slot.
pub const SLOT: u64 = 483_500_000;

/// Deterministic, visually distinguishable test pubkeys.
pub fn pk(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}

pub fn sig() -> Signature {
    Signature::from([7u8; 64])
}

pub fn sig_from(byte: u8) -> Signature {
    Signature::from([byte; 64])
}

/// `block_time` is a parameter because the two datasources differ on it: the RPC crawler
/// supplies it, the Yellowstone transaction stream leaves it `None`.
pub fn tx_metadata(
    signature: Signature,
    slot: u64,
    block_time: Option<i64>,
) -> Arc<TransactionMetadata> {
    Arc::new(TransactionMetadata {
        slot,
        signature,
        fee_payer: pk(0),
        meta: Default::default(),
        message: Default::default(),
        block_time,
        block_hash: None,
    })
}

pub fn instruction_metadata(
    transaction_metadata: Arc<TransactionMetadata>,
    absolute_path: &[u8],
) -> InstructionMetadata {
    InstructionMetadata {
        transaction_metadata,
        stack_height: absolute_path.len() as u32,
        index: absolute_path.first().copied().unwrap_or(0) as u32,
        absolute_path: absolute_path.to_vec(),
    }
}

pub fn decoded(
    data: XcavateWhitelistInstruction,
    accounts: &[Pubkey],
) -> DecodedInstruction<XcavateWhitelistInstruction> {
    decoded_for(carbon_xcavate_whitelist_decoder::PROGRAM_ID, data, accounts)
}

/// The generic form, for the sibling programs' decoded-instruction fixtures.
pub fn decoded_for<T>(program_id: Pubkey, data: T, accounts: &[Pubkey]) -> DecodedInstruction<T> {
    DecodedInstruction {
        program_id,
        data,
        accounts: accounts
            .iter()
            .map(|p| AccountMeta {
                pubkey: *p,
                is_signer: false,
                is_writable: false,
            })
            .collect(),
    }
}
