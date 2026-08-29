//! realxhub (v0.1.0) → SQL.
//!
//! realxhub is the fractional-hub-share program: hubs are created by the
//! config authority, holders earn income per share, and shares trade on a
//! per-seller listing (one live listing per seller per hub). It is the fifth
//! program onboarded to this indexer (ADR-30); the decoder is generated from
//! `idls/realxhub.json` and this mapper follows the same pattern as the other
//! four programs. The program is **not yet deployed** on devnet (ADR-30), so
//! its registry entry pins `deploy_slot: 0` and every state account that
//! appears before the first `initialize` is noise the batcher filters.
//!
//! Instructions
//!
//! | instruction  | action |
//! | ------------ | ------ |
//! | initialize   | —      |
//! | create_hub   | —      |
//! | faucet       | —      |
//! | list_shares  | —      |
//! | buy_shares   | — (conditional close: the seller's listing closes when the buy empties it) |
//! | delist_shares| — (closes the seller's listing) |
//! | claim_income | —      |
//! | record_sale  | —      |
//!
//! The program keeps no action rows (no bids/asks/orders): everything a
//! client needs is in the five state tables, which mirror the on-chain
//! accounts one-to-one.
//!
//! State tables (one per account type; `closed_at_slot` via
//! `db::realxhub::close_*` where an instruction destroys the account)
//!
//! | table                   | account      | closed by                  |
//! | ----------------------- | ------------ | -------------------------- |
//! | realxhub_config         | Config       | —                          |
//! | realxhub_faucet_receipt | FaucetReceipt| —                          |
//! | realxhub_holding        | Holding      | —                          |
//! | realxhub_hub            | Hub          | —                          |
//! | realxhub_share_listing  | ShareListing | `delist_shares`            |
//!
//! `buy_shares` also closes a listing — the seller's — but only when the buy
//! takes the last listed shares; the buy may also leave some shares listed
//! (a partial fill). The mapper can't know the pre-buy listing amount
//! (`decoded.accounts` holds *post*-instruction data), so `buy_shares` emits
//! `PendingClose::RealxhubShareListingIfEmptied` carrying the buyer's
//! `amount` and the listing pubkey; `db::realxhub::close_share_listing_if_emptied`
//! decides in SQL (listing closed iff its remaining amount equals the
//! amount just bought).

use carbon_core::account::{AccountDecoder, DecodedAccount};
use carbon_core::instruction::{DecodedInstruction, InstructionMetadata};
use carbon_realxhub_decoder::accounts::RealxhubAccount;
use carbon_realxhub_decoder::instructions::RealxhubInstruction;
use carbon_realxhub_decoder::{RealxhubDecoder, PROGRAM_ID};
use chrono::{DateTime, Utc};
use solana_account::Account;
use solana_pubkey::Pubkey;

use super::{
    account_bytes_at, close_at, instruction_row, ix_context, MappedInstruction, MappingError,
    PendingClose, ProgramMapper,
};
use crate::batcher::WriteOp;
use crate::db::close::StateTable;
use crate::db::realxhub::{
    RealxhubAccountRow, RealxhubConfigRow, RealxhubFaucetReceiptRow, RealxhubHoldingRow,
    RealxhubHubRow, RealxhubShareListingRow,
};

/// Maps realxhub instructions and accounts.
pub struct Realxhub;

impl ProgramMapper for Realxhub {
    type Ix = RealxhubInstruction;
    type Acc = RealxhubAccount;

    const NAME: &'static str = "realxhub";

    fn map_instruction(
        metadata: &InstructionMetadata,
        decoded: &DecodedInstruction<Self::Ix>,
        block_time: DateTime<Utc>,
    ) -> Result<Option<MappedInstruction>, MappingError> {
        map_instruction(metadata, decoded, block_time)
    }

    fn account_write_op(
        pubkey: Pubkey,
        slot: i64,
        lamports: i64,
        decoded: &DecodedAccount<Self::Acc>,
    ) -> WriteOp {
        account_write_op(pubkey, slot, lamports, decoded)
    }
}

/// snake_case instruction name for the `instructions` row (matches the IDL).
pub fn ix_name(ix: &RealxhubInstruction) -> &'static str {
    match ix {
        RealxhubInstruction::BuyShares(_) => "buy_shares",
        RealxhubInstruction::ClaimIncome(_) => "claim_income",
        RealxhubInstruction::CreateHub(_) => "create_hub",
        RealxhubInstruction::DelistShares(_) => "delist_shares",
        RealxhubInstruction::Faucet(_) => "faucet",
        RealxhubInstruction::Initialize(_) => "initialize",
        RealxhubInstruction::ListShares(_) => "list_shares",
        RealxhubInstruction::RecordSale(_) => "record_sale",
        RealxhubInstruction::CpiEvent(_) => "cpi_event",
    }
}

/// Decoded realxhub instruction → instruction row (+ pending state closes).
///
/// `delist_shares` closes the seller's listing (account index 2);
/// `buy_shares` conditionally closes it (index 3) when the buy empties it —
/// see the module docs for why the decision happens in the batcher.
pub fn map_instruction(
    metadata: &InstructionMetadata,
    decoded: &DecodedInstruction<RealxhubInstruction>,
    block_time: DateTime<Utc>,
) -> Result<Option<MappedInstruction>, MappingError> {
    let ix = &decoded.data;
    if matches!(ix, RealxhubInstruction::CpiEvent(_)) {
        return Ok(None);
    }
    let name = ix_name(ix);
    let accounts = decoded.accounts.as_slice();
    let ctx = ix_context(name, metadata)?;
    let slot = ctx.slot;
    let instruction = instruction_row(
        &PROGRAM_ID,
        name,
        metadata,
        &ctx,
        accounts,
        &decoded.data,
        block_time,
    )?;

    // Close tables: delist_shares → the seller's listing (index 2);
    // buy_shares → the same listing, but only when the buy empties it
    // (the batcher's `close_share_listing_if_emptied` checks the post-buy
    // amount in SQL, since the mapper can't read pre-instruction state).
    let closes = match ix {
        RealxhubInstruction::DelistShares(_) => {
            vec![close_at(
                accounts,
                2,
                name,
                StateTable::RealxhubShareListing,
                slot,
            )?]
        }
        RealxhubInstruction::BuyShares(args) => vec![PendingClose::RealxhubShareListingIfEmptied {
            pubkey: account_bytes_at(accounts, 3, name)?,
            bought_amount: args.amount as i64,
            slot,
        }],
        _ => vec![],
    };

    Ok(Some(MappedInstruction {
        instruction,
        action: None,
        closes,
        webhook_events: vec![],
    }))
}

/// Decoded state account → `UpsertRealxhubAccount` row.
///
/// u32/u64 → `i64` (max u32 fits comfortably; u64 → i64 only matters at
/// 2^63, unreachable for share counts and stablecoin lamports).
/// `u128` fields (`per_share`, `income_per_share`) are cumulative per-share
/// income in stablecoin base units and are stored as text so nothing is
/// lost; the API exposes them verbatim (ADR-30, ADR-10 — amounts that
/// aren't derivable from args alone).
pub fn account_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    decoded: &DecodedAccount<RealxhubAccount>,
) -> WriteOp {
    let pubkey = pubkey.to_bytes().to_vec();
    let row = match &decoded.data {
        RealxhubAccount::Config(c) => RealxhubAccountRow::Config(RealxhubConfigRow {
            pubkey: pubkey.clone(),
            slot,
            lamports,
            authority: c.authority.to_bytes().to_vec(),
            stable_mint: c.stable_mint.to_bytes().to_vec(),
            next_hub_id: c.next_hub_id as i64,
            bump: c.bump as i16,
        }),
        RealxhubAccount::FaucetReceipt(r) => {
            RealxhubAccountRow::FaucetReceipt(RealxhubFaucetReceiptRow {
                pubkey: pubkey.clone(),
                slot,
                lamports,
                last_drip: r.last_drip,
                bump: r.bump as i16,
            })
        }
        RealxhubAccount::Holding(h) => RealxhubAccountRow::Holding(RealxhubHoldingRow {
            pubkey: pubkey.clone(),
            slot,
            lamports,
            amount: h.amount as i64,
            listed: h.listed as i64,
            per_share: h.per_share.to_string(),
            pending: h.pending as i64,
            bump: h.bump as i16,
        }),
        RealxhubAccount::Hub(h) => RealxhubAccountRow::Hub(RealxhubHubRow {
            pubkey: pubkey.clone(),
            slot,
            lamports,
            id: h.id as i64,
            name: h.name.clone(),
            share_mint: h.share_mint.to_bytes().to_vec(),
            operational_spv: h.operational_spv.to_bytes().to_vec(),
            supplier: h.supplier.to_bytes().to_vec(),
            operators: h.operators.to_bytes().to_vec(),
            protocol: h.protocol.to_bytes().to_vec(),
            per_wallet_cap: h.per_wallet_cap as i64,
            income_per_share: h.income_per_share.to_string(),
            income_dust: h.income_dust as i64,
            bump: h.bump as i16,
        }),
        RealxhubAccount::ShareListing(l) => {
            RealxhubAccountRow::ShareListing(RealxhubShareListingRow {
                pubkey,
                slot,
                lamports,
                seller: l.seller.to_bytes().to_vec(),
                amount: l.amount as i64,
                price: l.price as i64,
                bump: l.bump as i16,
            })
        }
    };
    WriteOp::UpsertRealxhubAccount(row)
}

/// Decodes a state-account snapshot into its row write op.
///
/// `None` means the account is owned by the program but not decodable
/// (IDL drift — watch the `undecoded_accounts` counter).
pub fn snapshot_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    account: &Account,
) -> Option<WriteOp> {
    let decoded = RealxhubDecoder.decode_account(account)?;
    Some(account_write_op(pubkey, slot, lamports, &decoded))
}
