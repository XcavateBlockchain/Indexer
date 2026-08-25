//! The `property` program mapping: one `program_instructions` row per instruction (no action
//! log -- see the module docs on [`super`]), plus PendingCloses:
//!
//! | instruction | closes | at index |
//! |---|---|---|
//! | `close_agent_candidacy` | AgentCandidacy | 3 |
//! | `unlock_agent_votes` | AgentVote | 4 |
//! | `finalize_resignation` | ResignationNotice | 5 |
//! | `remove_letting_agent` | LettingAgent (CONDITIONAL) | 2 |
//! | `finalize_proposal` | Proposal | 4 |
//! | `unlock_proposal_votes` | GovVote | 4 |
//! | `finalize_challenge` | Challenge | 5 |
//! | `unlock_challenge_votes` | GovVote | 4 |
//! | `close_income_checkpoint` | IncomeCheckpoint | 2 |
//!
//! The unlock/close instructions are Anchor `close =` constraints; `finalize_proposal` /
//! `finalize_challenge` close their PDA by a runtime `close()` call that runs on every
//! successful transaction, so all of these are unconditional on success.
//! `remove_letting_agent` is conditional: on-chain its runtime `close()` fires only when
//! the removed location was the agent's last -- the mapper cannot know that (it is pure, no
//! DB), so it emits [`PendingClose::LettingAgentIfLast`] and the batcher's write
//! (`db::property::close_letting_agent_if_last`) decides against the stored row.
//! `finalize_challenge`'s optional `agent_entry` sits at index 6, AFTER the closed
//! challenge at 5, so the close index is stable.
//!
//! `propose` needs NO close arm despite its auto-approval path closing the just-created
//! Proposal: that create+close happens inside one instruction, so the account's
//! post-transaction state is already closed and never matches the owner-scoped account
//! filter -- no row is ever written for it, so there is nothing to close (unlike the
//! two-transaction same-slot tie `db::close` documents).

use carbon_core::account::{AccountDecoder, DecodedAccount};
use carbon_core::instruction::{DecodedInstruction, InstructionMetadata};
use carbon_property_decoder::accounts::PropertyAccount;
use carbon_property_decoder::instructions::PropertyInstruction;
use carbon_property_decoder::types::VoteChoice as ChainVoteChoice;
use carbon_property_decoder::{PropertyDecoder, PROGRAM_ID};
use chrono::{DateTime, Utc};
use solana_account::Account;
use solana_pubkey::Pubkey;

use super::{
    account_bytes_at, close_at, instruction_row, ix_context, MappedInstruction, MappingError,
    PendingClose, ProgramMapper,
};
use crate::batcher::WriteOp;
use crate::db::close::StateTable;
use crate::db::property::{
    AgentCandidacyRow, AgentVoteRow, ChallengeRow, GovVoteRow, IncomeCheckpointRow,
    LettingAgentRow, PropertyAccountRow, PropertyConfigRow, PropertyIncomeRow, PropertyLettingRow,
    ProposalRow, ResignationNoticeRow, VoteChoice,
};

/// The property program's [`ProgramMapper`] instantiation.
pub struct Property;

impl ProgramMapper for Property {
    type Ix = PropertyInstruction;
    type Acc = PropertyAccount;
    const NAME: &'static str = "property";

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
pub fn ix_name(ix: &PropertyInstruction) -> &'static str {
    match ix {
        PropertyInstruction::AcceptAuthority(_) => "accept_authority",
        PropertyInstruction::AddLettingAgent(_) => "add_letting_agent",
        PropertyInstruction::ChallengeAgent(_) => "challenge_agent",
        PropertyInstruction::ClaimIncome(_) => "claim_income",
        PropertyInstruction::ClaimProperty(_) => "claim_property",
        PropertyInstruction::CloseAgentCandidacy(_) => "close_agent_candidacy",
        PropertyInstruction::CloseIncomeCheckpoint(_) => "close_income_checkpoint",
        PropertyInstruction::DistributeIncome(_) => "distribute_income",
        PropertyInstruction::FinalizeAgentElection(_) => "finalize_agent_election",
        PropertyInstruction::FinalizeChallenge(_) => "finalize_challenge",
        PropertyInstruction::FinalizeProposal(_) => "finalize_proposal",
        PropertyInstruction::FinalizeResignation(_) => "finalize_resignation",
        PropertyInstruction::InitializeConfig(_) => "initialize_config",
        PropertyInstruction::Propose(_) => "propose",
        PropertyInstruction::RemoveLettingAgent(_) => "remove_letting_agent",
        PropertyInstruction::Resign(_) => "resign",
        PropertyInstruction::SettleIncome(_) => "settle_income",
        PropertyInstruction::UnlockAgentVotes(_) => "unlock_agent_votes",
        PropertyInstruction::UnlockChallengeVotes(_) => "unlock_challenge_votes",
        PropertyInstruction::UnlockProposalVotes(_) => "unlock_proposal_votes",
        PropertyInstruction::UpdateAuthority(_) => "update_authority",
        PropertyInstruction::UpdateConfig(_) => "update_config",
        PropertyInstruction::VoteOnAgent(_) => "vote_on_agent",
        PropertyInstruction::VoteOnChallenge(_) => "vote_on_challenge",
        PropertyInstruction::VoteOnProposal(_) => "vote_on_proposal",
        PropertyInstruction::CpiEvent(_) => "cpi_event",
    }
}

/// Map one decoded property instruction. `Ok(None)` only for the decoder's synthetic
/// `CpiEvent` variant (this program emits log-based `emit!`, never `emit_cpi!`).
pub fn map_instruction(
    metadata: &InstructionMetadata,
    decoded: &DecodedInstruction<PropertyInstruction>,
    block_time: DateTime<Utc>,
) -> Result<Option<MappedInstruction>, MappingError> {
    let name = ix_name(&decoded.data);

    if matches!(decoded.data, PropertyInstruction::CpiEvent(_)) {
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
        PropertyInstruction::CloseAgentCandidacy(_) => {
            vec![close_at(
                accounts,
                3,
                name,
                StateTable::PropertyAgentCandidacy,
                slot,
            )?]
        }
        PropertyInstruction::UnlockAgentVotes(_) => {
            vec![close_at(
                accounts,
                4,
                name,
                StateTable::PropertyAgentVote,
                slot,
            )?]
        }
        PropertyInstruction::FinalizeResignation(_) => {
            vec![close_at(
                accounts,
                5,
                name,
                StateTable::PropertyResignationNotice,
                slot,
            )?]
        }
        PropertyInstruction::RemoveLettingAgent(args) => {
            // The postcode arg identifies the removed location; the stored `locations` JSONB
            // keeps postcodes as UTF-8 strings (on-chain validated ASCII), so carry the same
            // shape for the batcher's comparison.
            vec![PendingClose::LettingAgentIfLast {
                pubkey: account_bytes_at(accounts, 2, name)?,
                removed_postcode: String::from_utf8_lossy(&args.postcode).into_owned(),
                slot,
            }]
        }
        PropertyInstruction::FinalizeProposal(_) => {
            vec![close_at(
                accounts,
                4,
                name,
                StateTable::PropertyProposal,
                slot,
            )?]
        }
        PropertyInstruction::UnlockProposalVotes(_)
        | PropertyInstruction::UnlockChallengeVotes(_) => {
            vec![close_at(
                accounts,
                4,
                name,
                StateTable::PropertyGovVote,
                slot,
            )?]
        }
        PropertyInstruction::FinalizeChallenge(_) => {
            vec![close_at(
                accounts,
                5,
                name,
                StateTable::PropertyChallenge,
                slot,
            )?]
        }
        PropertyInstruction::CloseIncomeCheckpoint(_) => vec![close_at(
            accounts,
            2,
            name,
            StateTable::PropertyIncomeCheckpoint,
            slot,
        )?],
        _ => vec![],
    };

    Ok(Some(MappedInstruction {
        instruction,
        action: None,
        closes,
    }))
}

fn vote_choice_from_chain(choice: &ChainVoteChoice) -> VoteChoice {
    match choice {
        ChainVoteChoice::Yes => VoteChoice::Yes,
        ChainVoteChoice::No => VoteChoice::No,
        ChainVoteChoice::Abstain => VoteChoice::Abstain,
    }
}

/// Decoded account -> state-table upsert (same contract as the whitelist's; see
/// [`super::whitelist::account_write_op`]).
///
/// `LettingAgent.locations` is serialized to the JSONB shape the migration documents
/// (postcodes as UTF-8 strings, NOT the decoder's serde byte arrays) -- the conditional
/// close's SQL comparison depends on this shape. `PropertyIncome.streams` /
/// `IncomeCheckpoint.entries` likewise take migration 0012's shapes, with the u128
/// `per_share` as a decimal string (serde_json's number type cannot carry the full range).
pub fn account_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    decoded: &DecodedAccount<PropertyAccount>,
) -> WriteOp {
    let pubkey = pubkey.to_bytes().to_vec();
    let row = match &decoded.data {
        // The IDL spells the account `property::state::Config` (namespaced because the
        // program now also imports the marketplace's Config type); the table stays
        // `property_config`.
        PropertyAccount::PropertyStateConfig(c) => PropertyAccountRow::Config(PropertyConfigRow {
            pubkey,
            slot,
            lamports,
            authority: c.authority.to_bytes().to_vec(),
            pending_authority: c.pending_authority.map(|p| p.to_bytes().to_vec()),
            xcav_mint: c.xcav_mint.to_bytes().to_vec(),
            treasury: c.treasury.to_bytes().to_vec(),
            rent_collector: c.rent_collector.to_bytes().to_vec(),
            agent_deposit: c.agent_deposit as i64,
            agent_voting_time: c.agent_voting_time,
            min_voting_quorum_bps: c.min_voting_quorum_bps as i32,
            agent_notice_period: c.agent_notice_period,
            proposal_voting_time: c.proposal_voting_time,
            low_proposal: c.low_proposal as i64,
            high_proposal: c.high_proposal as i64,
            high_threshold_bps: c.high_threshold_bps as i32,
            auto_approval_cooldown: c.auto_approval_cooldown,
            challenge_deposit: c.challenge_deposit as i64,
            agent_slash_amount: c.agent_slash_amount as i64,
            bump: c.bump as i16,
        }),
        PropertyAccount::AgentCandidacy(a) => {
            PropertyAccountRow::AgentCandidacy(AgentCandidacyRow {
                pubkey,
                slot,
                lamports,
                asset_id: a.asset_id as i64,
                round: a.round as i64,
                agent: a.agent.to_bytes().to_vec(),
                vote_power: a.vote_power as i64,
                rent_payer: a.rent_payer.to_bytes().to_vec(),
                bump: a.bump as i16,
            })
        }
        PropertyAccount::AgentVote(v) => PropertyAccountRow::AgentVote(AgentVoteRow {
            pubkey,
            slot,
            lamports,
            asset_id: v.asset_id as i64,
            round: v.round as i64,
            voter: v.voter.to_bytes().to_vec(),
            choice: v.choice.to_bytes().to_vec(),
            power: v.power as i64,
            rent_payer: v.rent_payer.to_bytes().to_vec(),
            bump: v.bump as i16,
        }),
        PropertyAccount::LettingAgent(a) => PropertyAccountRow::LettingAgent(LettingAgentRow {
            pubkey,
            slot,
            lamports,
            wallet: a.wallet.to_bytes().to_vec(),
            region_id: a.region_id as i32,
            locations: serde_json::Value::Array(
                a.locations
                    .iter()
                    .map(|l| {
                        serde_json::json!({
                            "postcode": String::from_utf8_lossy(&l.postcode).into_owned(),
                            "assigned_count": l.assigned_count,
                            "deposit": l.deposit,
                        })
                    })
                    .collect(),
            ),
            rent_payer: a.rent_payer.to_bytes().to_vec(),
            bump: a.bump as i16,
        }),
        PropertyAccount::PropertyLetting(l) => {
            PropertyAccountRow::PropertyLetting(PropertyLettingRow {
                pubkey,
                slot,
                lamports,
                asset_id: l.asset_id as i64,
                agent: l.agent.to_bytes().to_vec(),
                election_expiry: l.election.expiry,
                election_candidate_count: l.election.candidate_count as i64,
                election_round: l.election.round as i64,
                election_quorum_bps: l.election.quorum_bps as i32,
                governance_proposal_count: l.governance.proposal_count as i64,
                governance_challenge_count: l.governance.challenge_count as i64,
                governance_active_proposal: l.governance.active_proposal as i64,
                governance_active_challenge: l.governance.active_challenge as i64,
                governance_strikes: l.governance.strikes as i16,
                governance_last_auto_approval_ts: l.governance.last_auto_approval_ts,
                rent_payer: l.rent_payer.to_bytes().to_vec(),
                bump: l.bump as i16,
            })
        }
        PropertyAccount::Proposal(p) => PropertyAccountRow::Proposal(ProposalRow {
            pubkey,
            slot,
            lamports,
            asset_id: p.asset_id as i64,
            id: p.id as i64,
            proposer: p.proposer.to_bytes().to_vec(),
            amount: p.amount as i64,
            details_hash: p.details_hash.to_vec(),
            expiry: p.expiry,
            tally_yes: p.tally.yes as i64,
            tally_no: p.tally.no as i64,
            tally_abstain: p.tally.abstain as i64,
            quorum_bps: p.quorum_bps as i32,
            threshold_bps: p.threshold_bps as i32,
            rent_payer: p.rent_payer.to_bytes().to_vec(),
            bump: p.bump as i16,
        }),
        PropertyAccount::Challenge(c) => PropertyAccountRow::Challenge(ChallengeRow {
            pubkey,
            slot,
            lamports,
            asset_id: c.asset_id as i64,
            id: c.id as i64,
            challenger: c.challenger.to_bytes().to_vec(),
            agent: c.agent.to_bytes().to_vec(),
            deposit: c.deposit as i64,
            expiry: c.expiry,
            tally_yes: c.tally.yes as i64,
            tally_no: c.tally.no as i64,
            tally_abstain: c.tally.abstain as i64,
            quorum_bps: c.quorum_bps as i32,
            rent_payer: c.rent_payer.to_bytes().to_vec(),
            bump: c.bump as i16,
        }),
        PropertyAccount::GovVote(v) => PropertyAccountRow::GovVote(GovVoteRow {
            pubkey,
            slot,
            lamports,
            asset_id: v.asset_id as i64,
            id: v.id as i64,
            voter: v.voter.to_bytes().to_vec(),
            choice: vote_choice_from_chain(&v.choice),
            power: v.power as i64,
            rent_payer: v.rent_payer.to_bytes().to_vec(),
            bump: v.bump as i16,
        }),
        PropertyAccount::PropertyIncome(i) => PropertyAccountRow::Income(PropertyIncomeRow {
            pubkey,
            slot,
            lamports,
            asset_id: i.asset_id as i64,
            streams: serde_json::Value::Array(
                i.streams
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "mint": s.mint.to_string(),
                            "per_share": s.per_share.to_string(),
                            "dust": s.dust,
                        })
                    })
                    .collect(),
            ),
            rent_payer: i.rent_payer.to_bytes().to_vec(),
            bump: i.bump as i16,
        }),
        PropertyAccount::IncomeCheckpoint(c) => {
            PropertyAccountRow::IncomeCheckpoint(IncomeCheckpointRow {
                pubkey,
                slot,
                lamports,
                asset_id: c.asset_id as i64,
                owner: c.owner.to_bytes().to_vec(),
                entries: serde_json::Value::Array(
                    c.entries
                        .iter()
                        .map(|e| {
                            serde_json::json!({
                                "per_share": e.per_share.to_string(),
                                "pending": e.pending,
                            })
                        })
                        .collect(),
                ),
                rent_payer: c.rent_payer.to_bytes().to_vec(),
                bump: c.bump as i16,
            })
        }
        PropertyAccount::ResignationNotice(n) => {
            PropertyAccountRow::ResignationNotice(ResignationNoticeRow {
                pubkey,
                slot,
                lamports,
                asset_id: n.asset_id as i64,
                agent: n.agent.to_bytes().to_vec(),
                due_ts: n.due_ts,
                rent_payer: n.rent_payer.to_bytes().to_vec(),
                bump: n.bump as i16,
            })
        }
    };
    WriteOp::UpsertPropertyAccount(row)
}

/// Decode one `getProgramAccounts` result with this program's decoder and map it exactly like
/// a live account update. `None` = owned by the program but undecodable (IDL drift).
pub fn snapshot_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    account: &Account,
) -> Option<WriteOp> {
    let decoded = PropertyDecoder.decode_account(account)?;
    Some(account_write_op(pubkey, slot, lamports, &decoded))
}
