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
//!
//! The webhook producers (ADR-36), all tx-scoped event ids: `finalize_agent_election`
//! records `letting_agent_appointed` -- the on-chain appointment of a letting agent, which
//! the endpoint turns into write access on the property's legal bucket. The governance
//! lifecycle adds five more: `claim_property` (`agent_election_opened`, fires per
//! CANDIDACY -- the pure mapper cannot tell a round-opening claim from a round-joining
//! one, so the endpoint collapses them per round), `propose` (`proposal_created`, also on
//! the auto-approval path where no Proposal row survives -- the endpoint resolves via
//! GraphQL and treats a missing row as auto-approved), `finalize_proposal`
//! (`proposal_finalized`), `challenge_agent` (`challenge_created`), and
//! `finalize_challenge` (`challenge_finalized`). The finalize payloads carry the closing
//! PDA only; outcomes and tallies live in the frozen proposal/challenge rows and the
//! letting row (strikes, seat), which the endpoint reads -- the same ADR-36 accepted gap
//! as the appointment event.

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
    account_at, account_bytes_at, close_at, event_type, instruction_row, ix_context,
    MappedInstruction, MappingError, PendingClose, ProgramMapper, WebhookEvent,
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

    // ADR-36: `finalize_agent_election` appoints a letting agent to the property -- the
    // moment the endpoint adds the agent (write) to the property's legal bucket. Accounts:
    // letting 1, property 2 (the marketplace PropertyAsset PDA, a cross-program reference),
    // winner's LettingAgent entry 3 (trailing and OPTIONAL -- a missing index 3 must not
    // fail the mapping; the endpoint can resolve the winner from the letting row). Tx-scoped
    // event id: agents come and go (challenges, resignations), so appointments repeat per
    // property and each occurrence is its own event.
    let tx_signature = ctx.tx_signature.clone();
    let webhook_events = match &decoded.data {
        PropertyInstruction::FinalizeAgentElection(args) => {
            vec![WebhookEvent {
                event_id: format!("letting_agent_appointed:{tx_signature}:{}", ctx.index_str),
                event_type: event_type::LETTING_AGENT_APPOINTED,
                payload: serde_json::json!({
                    "event": event_type::LETTING_AGENT_APPOINTED,
                    "asset_id": args.asset_id,
                    "letting": account_at(accounts, 1, name)?,
                    "property": account_at(accounts, 2, name)?,
                    "winner_entry": accounts.get(3).map(|a| a.pubkey.to_string()),
                    "slot": slot,
                    "tx_signature": &tx_signature,
                    "block_time": block_time.to_rfc3339(),
                    "program": "property",
                }),
                slot,
                tx_signature,
                block_time,
            }]
        }
        // A letting agent stood for a seat election (accounts: agent 0, property 5,
        // letting 6, candidacy 7; the round is the `round` arg). Fires per candidacy --
        // the endpoint decides what merits a message.
        PropertyInstruction::ClaimProperty(args) => {
            vec![WebhookEvent {
                event_id: format!("agent_election_opened:{tx_signature}:{}", ctx.index_str),
                event_type: event_type::AGENT_ELECTION_OPENED,
                payload: serde_json::json!({
                    "event": event_type::AGENT_ELECTION_OPENED,
                    "property_id": args.asset_id,
                    "round": args.round,
                    "agent": account_at(accounts, 0, name)?,
                    "property": account_at(accounts, 5, name)?,
                    "letting": account_at(accounts, 6, name)?,
                    "candidacy": account_at(accounts, 7, name)?,
                    "slot": slot,
                    "tx_signature": &tx_signature,
                    "block_time": block_time.to_rfc3339(),
                    "program": "property",
                }),
                slot,
                tx_signature,
                block_time,
            }]
        }
        // The seated agent submitted a spending request (accounts: letting 4, proposal
        // 5). On the auto-approval path the Proposal PDA never survives the transaction,
        // so the webhook fires without a durable row behind it -- see the module doc.
        PropertyInstruction::Propose(args) => {
            vec![WebhookEvent {
                event_id: format!("proposal_created:{tx_signature}:{}", ctx.index_str),
                event_type: event_type::PROPOSAL_CREATED,
                payload: serde_json::json!({
                    "event": event_type::PROPOSAL_CREATED,
                    "property_id": args.asset_id,
                    "proposal_id": args.id,
                    "amount": args.amount,
                    "details_hash": bs58::encode(args.details_hash).into_string(),
                    "letting": account_at(accounts, 4, name)?,
                    "proposal": account_at(accounts, 5, name)?,
                    "slot": slot,
                    "tx_signature": &tx_signature,
                    "block_time": block_time.to_rfc3339(),
                    "program": "property",
                }),
                slot,
                tx_signature,
                block_time,
            }]
        }
        // A proposal vote closed; the PDA at index 4 is the one being closed (outcome +
        // tallies stay readable in the row's frozen state until then).
        PropertyInstruction::FinalizeProposal(args) => {
            vec![WebhookEvent {
                event_id: format!("proposal_finalized:{tx_signature}:{}", ctx.index_str),
                event_type: event_type::PROPOSAL_FINALIZED,
                payload: serde_json::json!({
                    "event": event_type::PROPOSAL_FINALIZED,
                    "property_id": args.asset_id,
                    "letting": account_at(accounts, 2, name)?,
                    "property": account_at(accounts, 3, name)?,
                    "proposal": account_at(accounts, 4, name)?,
                    "slot": slot,
                    "tx_signature": &tx_signature,
                    "block_time": block_time.to_rfc3339(),
                    "program": "property",
                }),
                slot,
                tx_signature,
                block_time,
            }]
        }
        // An investor challenged the seated agent (accounts: challenger 0, letting 5,
        // challenge 6).
        PropertyInstruction::ChallengeAgent(args) => {
            vec![WebhookEvent {
                event_id: format!("challenge_created:{tx_signature}:{}", ctx.index_str),
                event_type: event_type::CHALLENGE_CREATED,
                payload: serde_json::json!({
                    "event": event_type::CHALLENGE_CREATED,
                    "property_id": args.asset_id,
                    "challenge_id": args.id,
                    "challenger": account_at(accounts, 0, name)?,
                    "letting": account_at(accounts, 5, name)?,
                    "challenge": account_at(accounts, 6, name)?,
                    "slot": slot,
                    "tx_signature": &tx_signature,
                    "block_time": block_time.to_rfc3339(),
                    "program": "property",
                }),
                slot,
                tx_signature,
                block_time,
            }]
        }
        // A challenge vote closed; the PDA at index 5 is the one being closed, and the
        // optional agent entry at 6 tells the endpoint the seat's registry row (strike /
        // removal resolution happens row-side, like the appointment event).
        PropertyInstruction::FinalizeChallenge(args) => {
            vec![WebhookEvent {
                event_id: format!("challenge_finalized:{tx_signature}:{}", ctx.index_str),
                event_type: event_type::CHALLENGE_FINALIZED,
                payload: serde_json::json!({
                    "event": event_type::CHALLENGE_FINALIZED,
                    "property_id": args.asset_id,
                    "letting": account_at(accounts, 3, name)?,
                    "property": account_at(accounts, 4, name)?,
                    "challenge": account_at(accounts, 5, name)?,
                    "agent_entry": accounts.get(6).map(|a| a.pubkey.to_string()),
                    "slot": slot,
                    "tx_signature": &tx_signature,
                    "block_time": block_time.to_rfc3339(),
                    "program": "property",
                }),
                slot,
                tx_signature,
                block_time,
            }]
        }
        _ => vec![],
    };

    Ok(Some(MappedInstruction {
        instruction,
        action: None,
        closes,
        webhook_events,
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
