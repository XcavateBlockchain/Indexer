//! The `regions` program mapping: one `program_instructions` row per instruction (no action
//! log -- see the module docs on [`super`]), plus the PendingCloses read off the on-chain
//! source's `close =` constraints:
//!
//! | instruction | closes | at index |
//! |---|---|---|
//! | `create_region` | RegionState | 3 |
//! | `clear_region_state` | RegionState | 4 |
//! | `finalize_region_proposal` | RegionProposal | 5 |
//! | `remove_location` | Location | 7 |
//! | `unlock_voting_token` | VoteRecord | 5 |
//!
//! Note RegionState is closed at DIFFERENT positions by different instructions -- close
//! positions are per-instruction facts. All five closes are unconditional given the
//! instruction succeeded (failed transactions are never indexed).

use carbon_core::account::{AccountDecoder, DecodedAccount};
use carbon_core::instruction::{DecodedInstruction, InstructionMetadata};
use carbon_regions_decoder::accounts::RegionsAccount;
use carbon_regions_decoder::instructions::RegionsInstruction;
use carbon_regions_decoder::types::{RegionStatus as ChainRegionStatus, Vote as ChainVote};
use carbon_regions_decoder::{RegionsDecoder, PROGRAM_ID};
use chrono::{DateTime, Utc};
use solana_account::Account;
use solana_pubkey::Pubkey;

use super::{
    close_at, instruction_row, ix_context, MappedInstruction, MappingError, ProgramMapper,
};
use crate::batcher::WriteOp;
use crate::db::close::StateTable;
use crate::db::regions::{
    LocationRow, RegionProposalRow, RegionRow, RegionStateRow, RegionStatus, RegionsAccountRow,
    RegionsConfigRow, Vote, VoteRecordRow,
};

/// The regions program's [`ProgramMapper`] instantiation.
pub struct Regions;

impl ProgramMapper for Regions {
    type Ix = RegionsInstruction;
    type Acc = RegionsAccount;
    const NAME: &'static str = "regions";

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

/// The IDL spelling of an instruction, used verbatim as `program_instructions.ix_name`.
pub fn ix_name(ix: &RegionsInstruction) -> &'static str {
    match ix {
        RegionsInstruction::AcceptAuthority(_) => "accept_authority",
        RegionsInstruction::AdjustListingDuration(_) => "adjust_listing_duration",
        RegionsInstruction::AdjustRegionTax(_) => "adjust_region_tax",
        RegionsInstruction::ClaimOpenRegion(_) => "claim_open_region",
        RegionsInstruction::ClearRegionState(_) => "clear_region_state",
        RegionsInstruction::CreateNewLocation(_) => "create_new_location",
        RegionsInstruction::CreateRegion(_) => "create_region",
        RegionsInstruction::FinalizeRegionProposal(_) => "finalize_region_proposal",
        RegionsInstruction::InitializeConfig(_) => "initialize_config",
        RegionsInstruction::InitiateResignation(_) => "initiate_resignation",
        RegionsInstruction::ProposeNewRegion(_) => "propose_new_region",
        RegionsInstruction::RemoveLocation(_) => "remove_location",
        RegionsInstruction::UnlockVotingToken(_) => "unlock_voting_token",
        RegionsInstruction::UpdateAuthority(_) => "update_authority",
        RegionsInstruction::UpdateConfig(_) => "update_config",
        RegionsInstruction::VoteOnRegionProposal(_) => "vote_on_region_proposal",
        RegionsInstruction::CpiEvent(_) => "cpi_event",
    }
}

/// Map one decoded regions instruction. `Ok(None)` only for the decoder's synthetic
/// `CpiEvent` variant (this program emits log-based `emit!`, never `emit_cpi!`).
pub fn map_instruction(
    metadata: &InstructionMetadata,
    decoded: &DecodedInstruction<RegionsInstruction>,
    block_time: DateTime<Utc>,
) -> Result<Option<MappedInstruction>, MappingError> {
    let name = ix_name(&decoded.data);

    if matches!(decoded.data, RegionsInstruction::CpiEvent(_)) {
        return Ok(None);
    }

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

    let closes = match &decoded.data {
        RegionsInstruction::CreateRegion(_) => {
            vec![close_at(
                accounts,
                3,
                name,
                StateTable::RegionsRegionState,
                slot,
            )?]
        }
        RegionsInstruction::ClearRegionState(_) => {
            vec![close_at(
                accounts,
                4,
                name,
                StateTable::RegionsRegionState,
                slot,
            )?]
        }
        RegionsInstruction::FinalizeRegionProposal(_) => {
            // Closes the proposal on BOTH the pass and reject branches; the RegionState
            // account survives with its status flipped (do NOT close it here).
            vec![close_at(
                accounts,
                5,
                name,
                StateTable::RegionsRegionProposal,
                slot,
            )?]
        }
        RegionsInstruction::RemoveLocation(_) => {
            vec![close_at(
                accounts,
                7,
                name,
                StateTable::RegionsLocation,
                slot,
            )?]
        }
        RegionsInstruction::UnlockVotingToken(_) => {
            vec![close_at(
                accounts,
                5,
                name,
                StateTable::RegionsVoteRecord,
                slot,
            )?]
        }
        _ => vec![],
    };

    Ok(Some(MappedInstruction {
        instruction,
        action: None,
        closes,
        webhook_events: vec![],
    }))
}

fn status_from_chain(status: &ChainRegionStatus) -> RegionStatus {
    match status {
        ChainRegionStatus::Proposing => RegionStatus::Proposing,
        ChainRegionStatus::Passed => RegionStatus::Passed,
        ChainRegionStatus::Rejected => RegionStatus::Rejected,
    }
}

fn vote_from_chain(vote: &ChainVote) -> Vote {
    match vote {
        ChainVote::Yes => Vote::Yes,
        ChainVote::No => Vote::No,
        ChainVote::Abstain => Vote::Abstain,
    }
}

/// Decoded account -> state-table upsert (same contract as the whitelist's; see
/// [`super::whitelist::account_write_op`]).
pub fn account_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    decoded: &DecodedAccount<RegionsAccount>,
) -> WriteOp {
    let pubkey = pubkey.to_bytes().to_vec();
    let row = match &decoded.data {
        RegionsAccount::Config(c) => RegionsAccountRow::Config(RegionsConfigRow {
            pubkey,
            slot,
            lamports,
            authority: c.authority.to_bytes().to_vec(),
            pending_authority: c.pending_authority.map(|p| p.to_bytes().to_vec()),
            xcav_mint: c.xcav_mint.to_bytes().to_vec(),
            minimum_voting_amount: c.minimum_voting_amount as i64,
            voting_period: c.voting_period,
            owner_change_period: c.owner_change_period,
            threshold_bps: c.threshold_bps as i32,
            quorum: c.quorum as i64,
            notice_period: c.notice_period,
            min_vote_hold: c.min_vote_hold,
            max_listing_duration: c.max_listing_duration,
            max_tax_bps: c.max_tax_bps as i32,
            location_deposit: c.location_deposit as i64,
            proposal_counter: c.proposal_counter as i64,
            bump: c.bump as i16,
        }),
        RegionsAccount::Location(l) => RegionsAccountRow::Location(LocationRow {
            pubkey,
            slot,
            lamports,
            region_id: l.region_id as i32,
            postcode: l.postcode.clone(),
            deposit: l.deposit as i64,
            bump: l.bump as i16,
        }),
        RegionsAccount::Region(r) => RegionsAccountRow::Region(RegionRow {
            pubkey,
            slot,
            lamports,
            region_id: r.region_id as i32,
            owner: r.owner.to_bytes().to_vec(),
            collateral: r.collateral as i64,
            location_collateral: r.location_collateral as i64,
            next_owner_change: r.next_owner_change,
            listing_duration: r.listing_duration,
            tax_bps: r.tax_bps as i32,
            location_count: r.location_count as i64,
            bump: r.bump as i16,
        }),
        RegionsAccount::RegionProposal(p) => RegionsAccountRow::RegionProposal(RegionProposalRow {
            pubkey,
            slot,
            lamports,
            proposal_id: p.proposal_id as i64,
            proposer: p.proposer.to_bytes().to_vec(),
            region_id: p.region_id as i32,
            created_at: p.created_at,
            expiry: p.expiry,
            vote_cutoff: p.vote_cutoff,
            yes_power: p.yes_power as i64,
            no_power: p.no_power as i64,
            abstain_power: p.abstain_power as i64,
            bump: p.bump as i16,
        }),
        RegionsAccount::RegionState(s) => RegionsAccountRow::RegionState(RegionStateRow {
            pubkey,
            slot,
            lamports,
            region_id: s.region_id as i32,
            status: status_from_chain(&s.status),
            proposal_id: s.proposal_id as i64,
            proposer: s.proposer.to_bytes().to_vec(),
            deposit: s.deposit as i64,
            claim_deadline: s.claim_deadline,
            bump: s.bump as i16,
        }),
        RegionsAccount::VoteRecord(v) => RegionsAccountRow::VoteRecord(VoteRecordRow {
            pubkey,
            slot,
            lamports,
            proposal_id: v.proposal_id as i64,
            voter: v.voter.to_bytes().to_vec(),
            region_id: v.region_id as i32,
            vote: vote_from_chain(&v.vote),
            power: v.power as i64,
            expiry: v.expiry,
            bump: v.bump as i16,
        }),
    };
    WriteOp::UpsertRegionsAccount(row)
}

/// Decode one `getProgramAccounts` result with this program's decoder and map it exactly like
/// a live account update. `None` = owned by the program but undecodable (IDL drift).
pub fn snapshot_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    account: &Account,
) -> Option<WriteOp> {
    let decoded = RegionsDecoder.decode_account(account)?;
    Some(account_write_op(pubkey, slot, lamports, &decoded))
}
