//! The sibling programs' mapping contracts: close positions per closing instruction (the
//! per-instruction facts read off each program's on-chain `close =` constraints), the
//! no-action-log shape, and the CpiEvent no-op -- asserted the same way `mapping::tests`
//! asserts the whitelist's contract.

use carbon_core::instruction::{DecodedInstruction, InstructionMetadata};
use chrono::{DateTime, TimeZone, Utc};
use solana_pubkey::Pubkey;

use super::{MappedInstruction, PendingClose};
use crate::db::close::StateTable;
use crate::test_fixtures::{decoded_for, instruction_metadata, pk, sig, tx_metadata, SLOT};

fn block_time() -> DateTime<Utc> {
    Utc.timestamp_opt(1_750_000_000, 0).unwrap()
}

fn metadata() -> InstructionMetadata {
    instruction_metadata(tx_metadata(sig(), SLOT, None), &[0])
}

fn pks(n: usize) -> Vec<Pubkey> {
    (1..=n as u8).map(pk).collect()
}

fn expect_close(m: &MappedInstruction, table: StateTable, position: u8) {
    assert_eq!(
        m.closes,
        vec![PendingClose::Account {
            table,
            // `pks` numbers accounts 1.., so the account at list index N is pk(N + 1).
            pubkey: pk(position + 1).to_bytes().to_vec(),
            slot: SLOT as i64,
        }],
        "expected exactly one close of {table:?} at account index {position}"
    );
}

// --- regions --------------------------------------------------------------------------------

mod regions {
    use super::*;
    use crate::mapping::regions::map_instruction;
    use carbon_regions_decoder::instructions::{
        ClearRegionState, CreateRegion, FinalizeRegionProposal, ProposeNewRegion,
        RegionsInstruction, RemoveLocation, UnlockVotingToken,
    };
    use carbon_regions_decoder::PROGRAM_ID;

    fn map(data: RegionsInstruction, n_accounts: usize) -> MappedInstruction {
        map_instruction(
            &metadata(),
            &decoded_for(PROGRAM_ID, data, &pks(n_accounts)),
            block_time(),
        )
        .expect("mapping must succeed")
        .expect("this variant must produce rows")
    }

    #[test]
    fn create_region_closes_the_region_state_at_index_3() {
        let m = map(
            RegionsInstruction::CreateRegion(CreateRegion {
                region_id: 1,
                listing_duration: 60,
                tax_bps: 100,
            }),
            6,
        );
        assert_eq!(m.instruction.ix_name, "create_region");
        assert!(m.action.is_none(), "sibling programs have no action log");
        expect_close(&m, StateTable::RegionsRegionState, 3);
    }

    #[test]
    fn clear_region_state_closes_the_region_state_at_index_4() {
        // Same account type as create_region, DIFFERENT position -- the per-instruction
        // close positions must never be collapsed into a per-type constant.
        let m = map(
            RegionsInstruction::ClearRegionState(ClearRegionState { region_id: 1 }),
            10,
        );
        expect_close(&m, StateTable::RegionsRegionState, 4);
    }

    #[test]
    fn finalize_region_proposal_closes_only_the_proposal_at_index_5() {
        // The RegionState account (index 4 here) survives with its status flipped; closing
        // it too would be wrong.
        let m = map(
            RegionsInstruction::FinalizeRegionProposal(FinalizeRegionProposal { region_id: 1 }),
            11,
        );
        expect_close(&m, StateTable::RegionsRegionProposal, 5);
    }

    #[test]
    fn remove_location_closes_the_location_at_index_7() {
        let m = map(
            RegionsInstruction::RemoveLocation(RemoveLocation {
                region_id: 1,
                postcode: b"SW1A1AA".to_vec(),
            }),
            9,
        );
        expect_close(&m, StateTable::RegionsLocation, 7);
    }

    #[test]
    fn unlock_voting_token_closes_the_vote_record_at_index_5() {
        let m = map(
            RegionsInstruction::UnlockVotingToken(UnlockVotingToken { proposal_id: 3 }),
            7,
        );
        expect_close(&m, StateTable::RegionsVoteRecord, 5);
    }

    #[test]
    fn a_non_closing_instruction_closes_nothing_and_attributes_the_program() {
        let m = map(
            RegionsInstruction::ProposeNewRegion(ProposeNewRegion {
                region_id: 1,
                max_deposit: 10,
            }),
            8,
        );
        assert!(m.closes.is_empty());
        assert!(m.action.is_none());
        assert_eq!(m.instruction.program_id, PROGRAM_ID.to_bytes().to_vec());
        assert_eq!(m.instruction.ix_name, "propose_new_region");
    }
}

// --- property -------------------------------------------------------------------------------

mod property {
    use super::*;
    use crate::mapping::property::map_instruction;
    use carbon_property_decoder::instructions::{
        ChallengeAgent, CloseAgentCandidacy, CloseIncomeCheckpoint, DistributeIncome,
        FinalizeChallenge, FinalizeProposal, FinalizeResignation, PropertyInstruction, Propose,
        RemoveLettingAgent, UnlockAgentVotes, UnlockChallengeVotes, UnlockProposalVotes,
        VoteOnAgent,
    };
    use carbon_property_decoder::PROGRAM_ID;

    fn map(data: PropertyInstruction, n_accounts: usize) -> MappedInstruction {
        map_instruction(
            &metadata(),
            &decoded_for(PROGRAM_ID, data, &pks(n_accounts)),
            block_time(),
        )
        .expect("mapping must succeed")
        .expect("this variant must produce rows")
    }

    #[test]
    fn close_agent_candidacy_closes_the_candidacy_at_index_3() {
        let m = map(
            PropertyInstruction::CloseAgentCandidacy(CloseAgentCandidacy {
                asset_id: 1,
                round: 1,
                agent: pk(9),
            }),
            4,
        );
        expect_close(&m, StateTable::PropertyAgentCandidacy, 3);
    }

    #[test]
    fn unlock_agent_votes_closes_the_vote_record_at_index_4() {
        let m = map(
            PropertyInstruction::UnlockAgentVotes(UnlockAgentVotes {
                asset_id: 1,
                round: 1,
            }),
            7,
        );
        expect_close(&m, StateTable::PropertyAgentVote, 4);
    }

    #[test]
    fn finalize_resignation_closes_the_notice_at_index_5() {
        let m = map(
            PropertyInstruction::FinalizeResignation(FinalizeResignation { asset_id: 1 }),
            6,
        );
        expect_close(&m, StateTable::PropertyResignationNotice, 5);
    }

    #[test]
    fn remove_letting_agent_emits_the_conditional_close_with_the_postcode() {
        // The one close in the protocol that is NOT unconditional: on-chain the LettingAgent
        // PDA is closed only when the removed location was its last, so the mapper emits the
        // conditional op and the batcher's SQL decides against the stored row.
        let m = map(
            PropertyInstruction::RemoveLettingAgent(RemoveLettingAgent {
                postcode: b"M11AE".to_vec(),
            }),
            8,
        );
        assert_eq!(
            m.closes,
            vec![PendingClose::LettingAgentIfLast {
                pubkey: pk(3).to_bytes().to_vec(), // account index 2
                removed_postcode: "M11AE".to_string(),
                slot: SLOT as i64,
            }]
        );
    }

    #[test]
    fn vote_on_agent_closes_nothing() {
        let m = map(
            PropertyInstruction::VoteOnAgent(VoteOnAgent {
                asset_id: 1,
                amount: 5,
            }),
            9,
        );
        assert!(m.closes.is_empty());
        assert!(m.action.is_none());
        assert_eq!(m.instruction.ix_name, "vote_on_agent");
    }

    #[test]
    fn finalize_proposal_closes_the_proposal_at_index_4() {
        // A runtime `proposal.close(rent_payer)` at the end of the handler, executed on
        // every success -- unconditional from the mapping's point of view.
        let m = map(
            PropertyInstruction::FinalizeProposal(FinalizeProposal { asset_id: 1 }),
            5,
        );
        assert_eq!(m.instruction.ix_name, "finalize_proposal");
        expect_close(&m, StateTable::PropertyProposal, 4);
    }

    #[test]
    fn finalize_challenge_closes_the_challenge_at_index_5() {
        // The optional agent_entry sits at index 6, AFTER the closed challenge, so the
        // close index is stable whether or not the entry is passed.
        let m = map(
            PropertyInstruction::FinalizeChallenge(FinalizeChallenge { asset_id: 1 }),
            16,
        );
        assert_eq!(m.instruction.ix_name, "finalize_challenge");
        expect_close(&m, StateTable::PropertyChallenge, 5);
    }

    #[test]
    fn both_gov_unlocks_close_the_vote_record_at_index_4() {
        // Proposal votes and challenge votes are the same GovVote account type behind two
        // seed prefixes; both unlock instructions close it at the same position.
        for (name, data) in [
            (
                "unlock_proposal_votes",
                PropertyInstruction::UnlockProposalVotes(UnlockProposalVotes {
                    asset_id: 1,
                    id: 1,
                }),
            ),
            (
                "unlock_challenge_votes",
                PropertyInstruction::UnlockChallengeVotes(UnlockChallengeVotes {
                    asset_id: 1,
                    id: 1,
                }),
            ),
        ] {
            let m = map(data, 7);
            assert_eq!(m.instruction.ix_name, name);
            expect_close(&m, StateTable::PropertyGovVote, 4);
        }
    }

    #[test]
    fn close_income_checkpoint_closes_the_checkpoint_at_index_2() {
        let m = map(
            PropertyInstruction::CloseIncomeCheckpoint(CloseIncomeCheckpoint { asset_id: 1 }),
            4,
        );
        expect_close(&m, StateTable::PropertyIncomeCheckpoint, 2);
    }

    #[test]
    fn propose_closes_nothing_despite_the_auto_approval_path() {
        // An auto-approved (low-tier) proposal is created AND closed inside this one
        // instruction, so its post-transaction state never matches the owner-scoped account
        // filter and no row ever exists to close -- see the module doc's close table.
        let m = map(
            PropertyInstruction::Propose(Propose {
                asset_id: 1,
                id: 1,
                amount: 10,
                details_hash: [1; 32],
            }),
            7,
        );
        assert!(m.closes.is_empty());
        assert_eq!(m.instruction.ix_name, "propose");
    }

    #[test]
    fn challenge_agent_creates_without_closing() {
        let m = map(
            PropertyInstruction::ChallengeAgent(ChallengeAgent {
                asset_id: 1,
                id: 1,
                max_deposit: 100,
            }),
            12,
        );
        assert!(m.closes.is_empty());
        assert_eq!(m.instruction.ix_name, "challenge_agent");
    }

    #[test]
    fn distribute_income_emits_the_income_distributed_event() {
        // ADR-40: the agent paid rental income into the property's vault. Accounts 0/3/4/6/7
        // are the agent, the PropertyLetting, the (marketplace) PropertyAsset, the
        // PropertyIncome PDA, and the payment mint; the base-unit amount rides the arg.
        // Tx-scoped event id: distributions repeat per property.
        let m = map(
            PropertyInstruction::DistributeIncome(DistributeIncome {
                asset_id: 9,
                amount: 1_500_000,
            }),
            14,
        );
        assert_eq!(m.instruction.ix_name, "distribute_income");
        assert!(m.closes.is_empty());
        assert_eq!(m.webhook_events.len(), 1, "exactly one webhook event");
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_type, "income_distributed");
        assert_eq!(ev.event_id, format!("income_distributed:{}:0", sig()));
        assert_eq!(ev.slot, SLOT as i64);
        assert_eq!(ev.tx_signature, sig().to_string());
        assert_eq!(ev.block_time, block_time());
        let p = &ev.payload;
        assert_eq!(p["event"], "income_distributed");
        assert_eq!(p["asset_id"], serde_json::json!(9));
        assert_eq!(p["amount"], serde_json::json!(1_500_000));
        assert_eq!(p["agent"], pk(1).to_string()); // account index 0
        assert_eq!(p["letting"], pk(4).to_string()); // account index 3
        assert_eq!(p["property"], pk(5).to_string()); // account index 4
        assert_eq!(p["income"], pk(7).to_string()); // account index 6
        assert_eq!(p["mint"], pk(8).to_string()); // account index 7
        assert_eq!(p["tx_signature"], sig().to_string());
        assert_eq!(p["program"], "property");
    }

    #[test]
    fn distribute_income_needs_the_payment_mint_account() {
        // A truncated account list is a loud mapping error, never a dropped event: the
        // mint (index 7) and the other carried accounts are required.
        let result = map_instruction(
            &metadata(),
            &decoded_for(
                PROGRAM_ID,
                PropertyInstruction::DistributeIncome(DistributeIncome {
                    asset_id: 9,
                    amount: 1,
                }),
                &pks(7),
            ),
            block_time(),
        );
        assert!(result.is_err(), "missing account 7 must fail the mapping");
    }
}

// --- marketplace ----------------------------------------------------------------------------

mod marketplace {
    use super::*;
    use crate::mapping::marketplace::map_instruction;
    use carbon_marketplace_decoder::instructions::{
        AcceptOffer, AssignDeveloperLawyer, BuyPropertyShares, BuyRelistedShares, CancelOffer,
        ClaimShares, ClaimSpvCase, CloseCancelledPosition, CloseCase, CloseDeadListing,
        CloseShareHolding, DelistShares, ExecuteDeal, FinalizeSpvElection, InitPropertyAssets,
        LawyerConfirmDocuments, MakeOffer, MarketplaceInstruction, RejectOffer, ReleaseReservation,
        RelistShares, ReserveShares, ResolveSilentVerdict, SendPropertyShares, UnregisterLawyer,
        UnreserveShares, VoteOnSpvLawyer, WithdrawCancelled, WithdrawExpired,
        WithdrawLegalProcessExpired,
    };
    use carbon_marketplace_decoder::PROGRAM_ID;

    fn map(data: MarketplaceInstruction, n_accounts: usize) -> MappedInstruction {
        map_instruction(
            &metadata(),
            &decoded_for(PROGRAM_ID, data, &pks(n_accounts)),
            block_time(),
        )
        .expect("mapping must succeed")
        .expect("this variant must produce rows")
    }

    fn map_with_event_log(
        data: MarketplaceInstruction,
        n_accounts: usize,
        event_data: &str,
    ) -> MappedInstruction {
        map_with_event_log_at_path(data, n_accounts, event_data, &[0])
    }

    fn map_with_event_log_at_path(
        data: MarketplaceInstruction,
        n_accounts: usize,
        event_data: &str,
        path: &[u8],
    ) -> MappedInstruction {
        let mut logs = Vec::new();
        for top_level in 0..=path[0] {
            logs.push(format!("Program {PROGRAM_ID} invoke [1]"));
            if top_level == path[0] {
                if path.len() == 1 {
                    logs.push(format!("Program data: {event_data}"));
                } else {
                    for nested in 0..=path[1] {
                        logs.push(format!("Program {PROGRAM_ID} invoke [2]"));
                        if nested == path[1] {
                            logs.push(format!("Program data: {event_data}"));
                        }
                        logs.push(format!("Program {PROGRAM_ID} success"));
                    }
                }
            }
            logs.push(format!("Program {PROGRAM_ID} success"));
        }
        let mut transaction = tx_metadata(sig(), SLOT, None);
        std::sync::Arc::get_mut(&mut transaction)
            .expect("the fixture owns its transaction metadata")
            .meta
            .log_messages = Some(logs);
        map_instruction(
            &instruction_metadata(transaction, path),
            &decoded_for(PROGRAM_ID, data, &pks(n_accounts)),
            block_time(),
        )
        .expect("mapping must succeed")
        .expect("this variant must produce rows")
    }

    #[test]
    fn unregister_lawyer_closes_the_lawyer_at_index_3() {
        let m = map(
            MarketplaceInstruction::UnregisterLawyer(UnregisterLawyer {}),
            8,
        );
        expect_close(&m, StateTable::MarketplaceLawyer, 3);
    }

    #[test]
    fn release_reservation_closes_the_position_at_index_4() {
        let m = map(
            MarketplaceInstruction::ReleaseReservation(ReleaseReservation {
                listing_id: 1,
                investor: pk(9),
            }),
            6,
        );
        expect_close(&m, StateTable::MarketplaceInvestorPosition, 4);
    }

    #[test]
    fn close_cancelled_position_closes_the_position_at_index_4() {
        let m = map(
            MarketplaceInstruction::CloseCancelledPosition(CloseCancelledPosition {
                listing_id: 1,
                investor: pk(9),
            }),
            5,
        );
        expect_close(&m, StateTable::MarketplaceInvestorPosition, 4);
    }

    #[test]
    fn close_dead_listing_closes_both_the_listing_and_the_property_asset() {
        let m = map(
            MarketplaceInstruction::CloseDeadListing(CloseDeadListing { listing_id: 1 }),
            13,
        );
        assert_eq!(
            m.closes,
            vec![
                PendingClose::Account {
                    table: StateTable::MarketplaceListing,
                    pubkey: pk(5).to_bytes().to_vec(), // index 4
                    slot: SLOT as i64,
                },
                PendingClose::Account {
                    table: StateTable::MarketplacePropertyAsset,
                    pubkey: pk(6).to_bytes().to_vec(), // index 5
                    slot: SLOT as i64,
                },
            ]
        );
    }

    #[test]
    fn every_withdraw_variant_closes_the_position_and_the_holding_at_5_and_6() {
        // Same shared on-chain context (WithdrawExpired), three distinct instruction names --
        // and the InvestorPosition sits at index 5 here vs index 4 in release_reservation /
        // close_cancelled_position.
        for (name, data) in [
            (
                "withdraw_expired",
                MarketplaceInstruction::WithdrawExpired(WithdrawExpired { listing_id: 1 }),
            ),
            (
                "withdraw_legal_process_expired",
                MarketplaceInstruction::WithdrawLegalProcessExpired(WithdrawLegalProcessExpired {
                    listing_id: 1,
                }),
            ),
            (
                "withdraw_cancelled",
                MarketplaceInstruction::WithdrawCancelled(WithdrawCancelled { listing_id: 1 }),
            ),
        ] {
            let m = map(data, 18);
            assert_eq!(m.instruction.ix_name, name);
            assert_eq!(
                m.closes,
                vec![
                    PendingClose::Account {
                        table: StateTable::MarketplaceInvestorPosition,
                        pubkey: pk(6).to_bytes().to_vec(), // index 5
                        slot: SLOT as i64,
                    },
                    PendingClose::Account {
                        table: StateTable::MarketplaceShareHolding,
                        pubkey: pk(7).to_bytes().to_vec(), // index 6
                        slot: SLOT as i64,
                    },
                ],
                "{name}"
            );
        }
    }

    #[test]
    fn close_case_closes_nothing_despite_its_name() {
        let m = map(
            MarketplaceInstruction::CloseCase(CloseCase {
                listing_id: 1,
                lawyer: pk(9),
            }),
            5,
        );
        assert!(m.closes.is_empty());
    }

    #[test]
    fn a_short_account_list_on_a_closing_instruction_is_a_loud_error() {
        // close_dead_listing needs accounts[5]; give it four.
        let err = map_instruction(
            &metadata(),
            &decoded_for(
                PROGRAM_ID,
                MarketplaceInstruction::CloseDeadListing(CloseDeadListing { listing_id: 1 }),
                &pks(4),
            ),
            block_time(),
        )
        .expect_err("must not silently skip the close");
        assert_eq!(err.reason(), "missing_account");
    }

    #[test]
    fn a_plain_instruction_attributes_the_marketplace_program() {
        let m = map(
            MarketplaceInstruction::BuyPropertyShares(BuyPropertyShares {
                listing_id: 1,
                amount: 2,
                max_total_cost: 100,
            }),
            12,
        );
        assert!(m.closes.is_empty());
        assert!(m.action.is_none());
        assert_eq!(m.instruction.program_id, PROGRAM_ID.to_bytes().to_vec());
        assert_eq!(m.instruction.ix_name, "buy_property_shares");
    }

    #[test]
    fn accept_offer_closes_the_offer_and_conditionally_the_share_listing() {
        // The offer's runtime close runs on every success (index 10); the share listing
        // (index 6) closes on-chain only if the sale emptied it, by the OFFER's amount --
        // so the conditional op carries the offer's pubkey for the batcher's lookup.
        let m = map(
            MarketplaceInstruction::AcceptOffer(AcceptOffer { id: 1, nonce: 0 }),
            33,
        );
        assert_eq!(m.instruction.ix_name, "accept_offer");
        assert_eq!(
            m.closes,
            vec![
                PendingClose::Account {
                    table: StateTable::MarketplaceOffer,
                    pubkey: pk(11).to_bytes().to_vec(), // index 10
                    slot: SLOT as i64,
                },
                PendingClose::ShareListingIfEmptiedByOffer {
                    pubkey: pk(7).to_bytes().to_vec(),        // index 6
                    offer_pubkey: pk(11).to_bytes().to_vec(), // index 10
                    slot: SLOT as i64,
                },
            ]
        );
    }

    #[test]
    fn reject_offer_closes_the_offer_at_index_4() {
        let m = map(
            MarketplaceInstruction::RejectOffer(RejectOffer { id: 1, nonce: 0 }),
            13,
        );
        expect_close(&m, StateTable::MarketplaceOffer, 4);
    }

    #[test]
    fn cancel_offer_closes_the_offer_at_index_2() {
        // Same account type as reject_offer, DIFFERENT position -- per-instruction facts.
        let m = map(MarketplaceInstruction::CancelOffer(CancelOffer {}), 11);
        expect_close(&m, StateTable::MarketplaceOffer, 2);
    }

    #[test]
    fn buy_relisted_shares_emits_the_conditional_close_with_the_bought_amount() {
        // Closed on-chain only when this buy emptied the listing; the instruction's own
        // `amount` arg is the batcher's comparison value.
        let m = map(
            MarketplaceInstruction::BuyRelistedShares(BuyRelistedShares {
                asset_id: 1,
                id: 1,
                amount: 7,
                max_total_cost: 100,
            }),
            29,
        );
        assert_eq!(
            m.closes,
            vec![PendingClose::ShareListingIfEmptied {
                pubkey: pk(7).to_bytes().to_vec(), // index 6
                bought_amount: 7,
                slot: SLOT as i64,
            }]
        );
    }

    #[test]
    fn delist_shares_closes_the_share_listing_at_index_2() {
        let m = map(MarketplaceInstruction::DelistShares(DelistShares {}), 4);
        expect_close(&m, StateTable::MarketplaceShareListing, 2);
    }

    #[test]
    fn close_share_holding_closes_the_holding_at_index_4() {
        // ShareHolding sits at index 6 in the withdraw_* trio and index 4 here.
        let m = map(
            MarketplaceInstruction::CloseShareHolding(CloseShareHolding {}),
            5,
        );
        expect_close(&m, StateTable::MarketplaceShareHolding, 4);
    }

    #[test]
    fn claim_spv_case_emits_the_spv_case_claimed_event() {
        // Accounts 0/4/5/6 are the claiming lawyer's wallet, the listing, the
        // (marketplace) PropertyAsset, and the new candidacy. Fires per candidacy --
        // the endpoint collapses them per round -- so the id is transaction-scoped.
        let m = map(
            MarketplaceInstruction::ClaimSpvCase(ClaimSpvCase {
                listing_id: 9,
                round: 1,
                costs: 500,
            }),
            8,
        );
        assert_eq!(m.instruction.ix_name, "claim_spv_case");
        assert!(m.closes.is_empty());
        assert_eq!(m.webhook_events.len(), 1, "exactly one webhook event");
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_type, "spv_case_claimed");
        assert_eq!(ev.event_id, format!("spv_case_claimed:{}:0", sig()));
        let p = &ev.payload;
        assert_eq!(p["event"], "spv_case_claimed");
        assert_eq!(p["property_id"], serde_json::json!(9));
        assert_eq!(p["round"], serde_json::json!(1));
        assert_eq!(p["costs"], serde_json::json!(500));
        assert_eq!(p["lawyer"], pk(1).to_string()); // account index 0
        assert_eq!(p["listing"], pk(5).to_string()); // account index 4
        assert_eq!(p["property"], pk(6).to_string()); // account index 5
        assert_eq!(p["candidacy"], pk(7).to_string()); // account index 6
        assert_eq!(p["cluster"], "devnet");
        assert_eq!(p["program"], "marketplace");
    }

    #[test]
    fn the_non_closing_secondary_market_instructions_close_nothing() {
        for (name, data, n) in [
            (
                "make_offer",
                MarketplaceInstruction::MakeOffer(MakeOffer {
                    id: 1,
                    amount: 2,
                    share_price: 10,
                }),
                13,
            ),
            (
                "relist_shares",
                MarketplaceInstruction::RelistShares(RelistShares {
                    asset_id: 1,
                    amount: 2,
                    share_price: 10,
                }),
                8,
            ),
            (
                "send_property_shares",
                MarketplaceInstruction::SendPropertyShares(SendPropertyShares {
                    asset_id: 1,
                    amount: 2,
                }),
                21,
            ),
        ] {
            let m = map(data, n);
            assert!(m.closes.is_empty(), "{name}");
            assert_eq!(m.instruction.ix_name, name);
        }
    }

    #[test]
    fn init_property_assets_emits_the_property_asset_webhook_event() {
        // ADR-28: asset registration is the moment the `PropertyAsset` PDA (account index 3)
        // gets its name + metadata_uri + share mint (index 4) -- the one instruction that
        // fires the outbound webhook, deduped by the asset PDA.
        let m = map(
            MarketplaceInstruction::InitPropertyAssets(InitPropertyAssets {
                listing_id: 1,
                name: "42 Main Street".to_string(),
                uri: "https://metadata.example/42-main".to_string(),
            }),
            11,
        );
        assert_eq!(m.instruction.ix_name, "init_property_assets");
        assert_eq!(m.webhook_events.len(), 1, "exactly one webhook event");
        let ev = &m.webhook_events[0];
        let property_b58 = bs58::encode(pk(4).to_bytes()).into_string(); // index 3
        let share_mint_b58 = bs58::encode(pk(5).to_bytes()).into_string(); // index 4
        assert_eq!(ev.event_type, "property_asset_registered");
        assert_eq!(
            ev.event_id,
            format!("property_asset_registered:{property_b58}")
        );
        assert_eq!(ev.slot, SLOT as i64);
        assert_eq!(ev.tx_signature, sig().to_string());
        assert_eq!(ev.block_time, block_time());
        let p = &ev.payload;
        assert_eq!(p["event"], "property_asset_registered");
        assert_eq!(p["pubkey"], property_b58);
        assert_eq!(p["listing_id"], serde_json::json!(1));
        assert_eq!(p["asset_id"], serde_json::json!(1));
        assert_eq!(p["name"], "42 Main Street");
        assert_eq!(p["metadata_uri"], "https://metadata.example/42-main");
        assert_eq!(p["share_mint"], share_mint_b58);
        assert_eq!(p["slot"], serde_json::json!(SLOT as i64));
        assert_eq!(p["tx_signature"], sig().to_string());
        assert_eq!(p["block_time"], block_time().to_rfc3339());
        assert_eq!(p["program"], "marketplace");
    }

    #[test]
    fn claim_shares_emits_only_the_offers_namespace_event() {
        let m = map(
            MarketplaceInstruction::ClaimShares(ClaimShares { listing_id: 7 }),
            8,
        );
        assert_eq!(m.instruction.ix_name, "claim_shares");
        assert_eq!(m.webhook_events.len(), 1);
        assert!(m
            .webhook_events
            .iter()
            .all(|event| event.event_type != "property_claim_started"));

        let listing_b58 = pk(5).to_string(); // account index 4
        let offers_ev = &m.webhook_events[0];
        assert_eq!(
            offers_ev.event_id,
            format!("offers_namespace_created:{listing_b58}")
        );
        assert_eq!(offers_ev.payload["event"], "offers_namespace_created");
        assert_eq!(offers_ev.payload["listing_id"], serde_json::json!(7));
        assert_eq!(offers_ev.payload["listing"], listing_b58);
        assert_eq!(offers_ev.payload["property"], pk(6).to_string());
        assert_eq!(offers_ev.payload["asset_id"], serde_json::json!(7));
        assert_eq!(offers_ev.payload["cluster"], "devnet");
        assert_eq!(offers_ev.payload["program"], "marketplace");
    }

    #[test]
    fn relist_shares_emits_the_offers_namespace_created_event() {
        // A seller relists shares on the secondary market. The event is transaction-scoped
        // so a later delist + relist of the same share listing is not dropped.
        let m = map(
            MarketplaceInstruction::RelistShares(RelistShares {
                asset_id: 9,
                amount: 5,
                share_price: 1_000_000,
            }),
            9,
        );
        assert_eq!(m.instruction.ix_name, "relist_shares");
        assert_eq!(m.webhook_events.len(), 1);
        let ev = &m.webhook_events[0];
        let listing_b58 = pk(5).to_string(); // account index 4
        let share_listing_b58 = pk(7).to_string(); // account index 6
        assert_eq!(ev.event_type, "offers_namespace_created");
        assert_eq!(ev.event_id, format!("offers_namespace_created:{}:0", sig()));
        let p = &ev.payload;
        assert_eq!(p["event"], "offers_namespace_created");
        assert_eq!(p["asset_id"], serde_json::json!(9));
        assert_eq!(p["listing_id"], serde_json::json!(9));
        assert_eq!(p["listing"], listing_b58);
        assert_eq!(p["share_listing"], share_listing_b58);
        assert_eq!(p["seller"], pk(1).to_string()); // account index 0
        assert_eq!(p["amount"], serde_json::json!(5));
        assert_eq!(p["share_price"], serde_json::json!(1_000_000));
        assert_eq!(p["cluster"], "devnet");
        assert_eq!(p["program"], "marketplace");
    }

    #[test]
    fn assign_developer_lawyer_emits_the_developer_lawyer_assigned_event() {
        // ADR-48: accounts 3/4/5 are the lawyer registry, listing, and property PDAs. The
        // transaction-scoped event and the SPV-election event drive the same downstream bucket.
        let m = map(
            MarketplaceInstruction::AssignDeveloperLawyer(AssignDeveloperLawyer {
                listing_id: 7,
                lawyer: pk(20),
            }),
            6,
        );
        assert_eq!(m.instruction.ix_name, "assign_developer_lawyer");
        assert_eq!(m.webhook_events.len(), 1);
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_type, "developer_lawyer_assigned");
        assert_eq!(
            ev.event_id,
            format!("developer_lawyer_assigned:{}:0", sig())
        );
        assert_eq!(ev.slot, SLOT as i64);
        assert_eq!(ev.tx_signature, sig().to_string());
        assert_eq!(ev.block_time, block_time());
        let p = &ev.payload;
        assert_eq!(p["event"], "developer_lawyer_assigned");
        assert_eq!(p["listing_id"], serde_json::json!(7));
        assert_eq!(p["property_id"], serde_json::json!(7));
        assert_eq!(p["developer_lawyer"], pk(20).to_string());
        assert_eq!(p["developer_lawyer_registry"], pk(4).to_string());
        assert_eq!(p["listing"], pk(5).to_string());
        assert_eq!(p["property"], pk(6).to_string());
        assert_eq!(p["cluster"], "devnet");
        assert_eq!(p["slot"], serde_json::json!(SLOT as i64));
        assert_eq!(p["tx_signature"], sig().to_string());
        assert_eq!(p["block_time"], block_time().to_rfc3339());
        assert_eq!(p["program"], "marketplace");
    }

    #[test]
    fn vote_on_spv_lawyer_emits_the_voter_and_candidacy_event() {
        let m = map(
            MarketplaceInstruction::VoteOnSpvLawyer(VoteOnSpvLawyer {
                listing_id: 7,
                amount: 25,
            }),
            9,
        );

        assert_eq!(m.instruction.ix_name, "vote_on_spv_lawyer");
        assert!(m.closes.is_empty());
        assert_eq!(m.webhook_events.len(), 1);
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_type, "spv_lawyer_vote_cast");
        assert_eq!(ev.event_id, format!("spv_lawyer_vote_cast:{}:0", sig()));
        assert_eq!(ev.slot, SLOT as i64);
        assert_eq!(ev.tx_signature, sig().to_string());
        assert_eq!(ev.block_time, block_time());
        let p = &ev.payload;
        assert_eq!(p["event"], "spv_lawyer_vote_cast");
        assert_eq!(p["property_id"], serde_json::json!(7));
        assert_eq!(p["voter"], pk(1).to_string()); // account 0, not payer account 1
        assert_eq!(p["candidacy"], pk(7).to_string()); // account index 6
        assert_eq!(p["amount"], serde_json::json!(25));
        assert_eq!(p["listing"], pk(4).to_string()); // account index 3
        assert_eq!(p["cluster"], "devnet");
        assert_eq!(p["slot"], serde_json::json!(SLOT as i64));
        assert_eq!(p["tx_signature"], sig().to_string());
        assert_eq!(p["instruction_path"], "0");
        assert_eq!(p["block_time"], block_time().to_rfc3339());
        assert_eq!(p["program"], "marketplace");
    }

    #[test]
    fn finalize_spv_election_emits_the_lawyer_elected_event() {
        // ADR-36: accounts 1/2 are the listing + property PDAs; account 3 is the winner's
        // Lawyer registry (optional). The event id is transaction-scoped: a second election
        // round (after a failed legal process) must fire again.
        let m = map(
            MarketplaceInstruction::FinalizeSpvElection(FinalizeSpvElection { listing_id: 7 }),
            4,
        );
        assert_eq!(m.instruction.ix_name, "finalize_spv_election");
        assert_eq!(m.webhook_events.len(), 1);
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_type, "spv_lawyer_elected");
        assert_eq!(ev.event_id, format!("spv_lawyer_elected:{}:0", sig()));
        let p = &ev.payload;
        assert_eq!(p["event"], "spv_lawyer_elected");
        assert_eq!(p["listing_id"], serde_json::json!(7));
        assert_eq!(p["property_id"], serde_json::json!(7));
        assert_eq!(p["listing"], pk(2).to_string()); // account index 1
        assert_eq!(p["property"], pk(3).to_string()); // account index 2
        assert_eq!(p["cluster"], "devnet");
        assert_eq!(p["winner_registry"], pk(4).to_string()); // account index 3
    }

    #[test]
    fn finalize_spv_election_tolerates_a_missing_optional_winner_registry() {
        // The winner registry is a trailing OPTIONAL account: a 3-account instruction still
        // maps, with a null `winner_registry` the endpoint resolves from the listing row.
        let m = map(
            MarketplaceInstruction::FinalizeSpvElection(FinalizeSpvElection { listing_id: 7 }),
            3,
        );
        let ev = &m.webhook_events[0];
        assert_eq!(ev.payload["winner_registry"], serde_json::Value::Null);
    }

    /// Map one instruction against a caller-supplied log list, so a test can place an
    /// unrelated or nested log line inside the SAME instruction's logs.
    fn map_with_logs(
        data: MarketplaceInstruction,
        n_accounts: usize,
        path: &[u8],
        logs: Vec<String>,
    ) -> MappedInstruction {
        let mut transaction = tx_metadata(sig(), SLOT, None);
        std::sync::Arc::get_mut(&mut transaction)
            .expect("the fixture owns its transaction metadata")
            .meta
            .log_messages = Some(logs);
        map_instruction(
            &instruction_metadata(transaction, path),
            &decoded_for(PROGRAM_ID, data, &pks(n_accounts)),
            block_time(),
        )
        .expect("mapping must succeed")
        .expect("this variant must produce rows")
    }

    /// The same `lawyer_confirm_documents` instruction twice in ONE transaction, each emit
    /// inside its own invoke frame. ADR-52: the transaction signature alone cannot identify a
    /// verdict, so the two mappings must produce two distinct outbox rows.
    fn map_two_confirmations_in_one_transaction(
        first_event: &str,
        second_event: &str,
    ) -> (MappedInstruction, MappedInstruction) {
        let logs = vec![
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!("Program data: {first_event}"),
            format!("Program {PROGRAM_ID} success"),
            format!("Program {PROGRAM_ID} invoke [1]"),
            format!("Program data: {second_event}"),
            format!("Program {PROGRAM_ID} success"),
        ];
        let mut transaction = tx_metadata(sig(), SLOT, None);
        std::sync::Arc::get_mut(&mut transaction)
            .expect("the fixture owns its transaction metadata")
            .meta
            .log_messages = Some(logs);
        let map_at = |path: &[u8]| {
            map_instruction(
                &instruction_metadata(std::sync::Arc::clone(&transaction), path),
                &decoded_for(
                    PROGRAM_ID,
                    MarketplaceInstruction::LawyerConfirmDocuments(LawyerConfirmDocuments {
                        listing_id: 7,
                        approve: true,
                        documents_hash: [9; 32],
                    }),
                    &pks(3),
                ),
                block_time(),
            )
            .expect("mapping must succeed")
            .expect("this variant must produce rows")
        };
        (map_at(&[0]), map_at(&[1]))
    }

    /// The pinned v2 key set (`sorted`), so a payload field can only change by an explicit
    /// contract edit -- in particular the removed `resolution_required` / `post_state` can
    /// never silently return.
    fn sorted_keys(payload: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = payload
            .as_object()
            .expect("a webhook payload is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        keys
    }

    const DOCUMENTS_CONFIRMED_KEYS: [&str; 17] = [
        "approve",
        "block_time",
        "cluster",
        "documents_hash",
        "event",
        "instruction_path",
        "lawyer",
        "listing",
        "listing_id",
        "log_evidence",
        "payload_version",
        "program",
        "property_id",
        "review_outcome",
        "slot",
        "status",
        "tx_signature",
    ];

    const SILENT_VERDICT_RESOLVED_KEYS: [&str; 15] = [
        "block_time",
        "cluster",
        "event",
        "instruction_path",
        "listing",
        "listing_id",
        "log_evidence",
        "payload_version",
        "program",
        "property_id",
        "review_outcome",
        "silent_lawyer",
        "slot",
        "status",
        "tx_signature",
    ];

    /// The `DocumentsConfirmed` log emits, base64-encoded (discriminator + listing_id 7 +
    /// lawyer `[1; 32]` + approve + hash `[9; 32]` + the resulting `ListingStatus`).
    const LEGAL_LOG: &str = "V1Jwgqou52IHAAAAAAAAAAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJAw==";
    const SOLD_OUT_LOG: &str = "V1Jwgqou52IHAAAAAAAAAAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJAg==";
    const CANCELLED_LOG: &str = "V1Jwgqou52IHAAAAAAAAAAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJBg==";

    fn confirm_documents() -> MarketplaceInstruction {
        MarketplaceInstruction::LawyerConfirmDocuments(LawyerConfirmDocuments {
            listing_id: 7,
            approve: true,
            documents_hash: [9; 32],
        })
    }

    /// The whole v2 `documents_confirmed` payload, pinned by value (not field by field) so the
    /// ProfileApi contract fixture can be aligned byte-for-byte. Field order is irrelevant on
    /// the wire; the KEY SET and every value are the contract.
    #[test]
    fn documents_confirmed_v2_payload_is_pinned_whole() {
        let m = map_with_event_log(confirm_documents(), 3, LEGAL_LOG);
        assert_eq!(
            m.webhook_events[0].payload,
            serde_json::json!({
                "event": "documents_confirmed",
                "payload_version": 2,
                "log_evidence": "decoded",
                "review_outcome": "approved",
                "listing_id": 7,
                "property_id": 7,
                "cluster": "devnet",
                "lawyer": pk(1).to_string(),
                "approve": true,
                "documents_hash": bs58::encode([9u8; 32]).into_string(),
                "status": "Legal",
                "listing": pk(3).to_string(),
                "slot": SLOT as i64,
                "tx_signature": sig().to_string(),
                "instruction_path": "0",
                "block_time": block_time().to_rfc3339(),
                "program": "marketplace",
            })
        );
    }

    /// The whole v2 `silent_verdict_resolved` payload, pinned by value for the same reason.
    #[test]
    fn silent_verdict_resolved_v2_payload_is_pinned_whole() {
        let m = map_with_event_log(
            MarketplaceInstruction::ResolveSilentVerdict(ResolveSilentVerdict { listing_id: 7 }),
            2,
            "Js+sjGC2z3MHAAAAAAAAABUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUV",
        );
        assert_eq!(
            m.webhook_events[0].payload,
            serde_json::json!({
                "event": "silent_verdict_resolved",
                "payload_version": 2,
                "log_evidence": "decoded",
                "review_outcome": "cancelled",
                "listing_id": 7,
                "property_id": 7,
                "cluster": "devnet",
                "silent_lawyer": pk(21).to_string(),
                "status": "Cancelled",
                "listing": pk(2).to_string(),
                "slot": SLOT as i64,
                "tx_signature": sig().to_string(),
                "instruction_path": "0",
                "block_time": block_time().to_rfc3339(),
                "program": "marketplace",
            })
        );
    }

    #[test]
    fn lawyer_confirm_documents_emits_a_redelivery_safe_verdict_event() {
        let m = map(confirm_documents(), 3);
        assert_eq!(m.instruction.ix_name, "lawyer_confirm_documents");
        assert_eq!(m.webhook_events.len(), 1);
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_type, "documents_confirmed");
        assert_eq!(ev.event_id, format!("documents_confirmed:{}:0", sig()));
        assert_eq!(ev.slot, SLOT as i64);
        assert_eq!(ev.tx_signature, sig().to_string());
        assert_eq!(ev.block_time, block_time());
        let p = &ev.payload;
        assert_eq!(
            sorted_keys(p),
            DOCUMENTS_CONFIRMED_KEYS,
            "the pinned v2 documents_confirmed key set"
        );
        assert_eq!(p["event"], "documents_confirmed");
        assert_eq!(p["listing_id"], serde_json::json!(7));
        assert_eq!(p["property_id"], serde_json::json!(7));
        assert_eq!(p["lawyer"], pk(1).to_string()); // account index 0
        assert_eq!(p["approve"], serde_json::json!(true));
        // The wire form is base58 of the 32 instruction bytes, never hex.
        assert_eq!(p["documents_hash"], bs58::encode([9; 32]).into_string());
        assert_eq!(p["listing"], pk(3).to_string()); // account index 2
        assert_eq!(p["cluster"], "devnet");
        assert!(
            p.get("status").is_some(),
            "status must be present even when null"
        );
        assert_eq!(p["status"], serde_json::Value::Null);
        assert_eq!(p["payload_version"], 2);
        assert_eq!(p["log_evidence"], "missing");
        assert_eq!(p["review_outcome"], "unresolved");
        assert_eq!(p["instruction_path"], "0");
        assert_eq!(p["slot"], serde_json::json!(SLOT as i64));
        assert_eq!(p["tx_signature"], sig().to_string());
        assert_eq!(p["block_time"], block_time().to_rfc3339());
        assert_eq!(p["program"], "marketplace");

        let redelivered = map(confirm_documents(), 3);
        assert_eq!(redelivered.webhook_events[0].event_id, ev.event_id);
    }

    #[test]
    fn lawyer_confirm_documents_preserves_the_resulting_chain_status() {
        // ADR-52: `status` is the program's own post-instruction ListingStatus and
        // `review_outcome` is its label. SoldOut is reported as-is -- it covers BOTH a pending
        // second seat and the first split reset, which only the consumer's ordered verdict
        // ledger can tell apart -- and it is delivered like any other event.
        for (event_data, expected_status, expected_outcome) in [
            (LEGAL_LOG, "Legal", "approved"),
            (SOLD_OUT_LOG, "SoldOut", "unresolved"),
            (CANCELLED_LOG, "Cancelled", "cancelled"),
        ] {
            let m = map_with_event_log(confirm_documents(), 3, event_data);
            assert_eq!(m.webhook_events.len(), 1);
            let p = &m.webhook_events[0].payload;
            assert_eq!(
                sorted_keys(p),
                DOCUMENTS_CONFIRMED_KEYS,
                "the pinned v2 documents_confirmed key set"
            );
            assert_eq!(p["status"], expected_status);
            assert_eq!(p["payload_version"], 2);
            assert_eq!(p["log_evidence"], "decoded");
            assert_eq!(p["review_outcome"], expected_outcome);
        }
    }

    #[test]
    fn two_verdicts_in_one_transaction_are_two_distinct_events() {
        // Two assigned lawyers can rule in one transaction. The outbox id and the consumer's
        // dedupe key are therefore (event, tx_signature, instruction_path) -- one signature
        // carrying two verdicts must not collapse into one row.
        let (first, second) = map_two_confirmations_in_one_transaction(LEGAL_LOG, CANCELLED_LOG);
        assert_eq!(first.webhook_events.len(), 1);
        assert_eq!(second.webhook_events.len(), 1);
        let first = &first.webhook_events[0];
        let second = &second.webhook_events[0];

        assert_eq!(first.event_id, format!("documents_confirmed:{}:0", sig()));
        assert_eq!(second.event_id, format!("documents_confirmed:{}:1", sig()));
        assert_ne!(first.event_id, second.event_id);
        // Same transaction identity, different instruction path and different log status.
        assert_eq!(first.tx_signature, second.tx_signature);
        assert_eq!(
            first.payload["tx_signature"],
            second.payload["tx_signature"]
        );
        assert_eq!(first.payload["instruction_path"], "0");
        assert_eq!(second.payload["instruction_path"], "1");
        assert_eq!(first.payload["log_evidence"], "decoded");
        assert_eq!(second.payload["log_evidence"], "decoded");
        assert_eq!(first.payload["status"], "Legal");
        assert_eq!(second.payload["status"], "Cancelled");
        assert_eq!(first.payload["review_outcome"], "approved");
        assert_eq!(second.payload["review_outcome"], "cancelled");
        assert_ne!(first.payload, second.payload);
    }

    #[test]
    fn resolve_silent_verdict_emits_the_silent_lawyer_and_cancellation() {
        let m = map_with_event_log(
            MarketplaceInstruction::ResolveSilentVerdict(ResolveSilentVerdict { listing_id: 7 }),
            2,
            "Js+sjGC2z3MHAAAAAAAAABUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUV",
        );
        assert_eq!(m.instruction.ix_name, "resolve_silent_verdict");
        assert_eq!(m.webhook_events.len(), 1);
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_type, "silent_verdict_resolved");
        assert_eq!(ev.event_id, format!("silent_verdict_resolved:{}:0", sig()));
        let p = &ev.payload;
        assert_eq!(
            sorted_keys(p),
            SILENT_VERDICT_RESOLVED_KEYS,
            "the pinned v2 silent_verdict_resolved key set"
        );
        assert_eq!(p["event"], "silent_verdict_resolved");
        assert_eq!(p["listing_id"], serde_json::json!(7));
        assert_eq!(p["property_id"], serde_json::json!(7));
        assert_eq!(p["silent_lawyer"], pk(21).to_string());
        assert_eq!(p["status"], "Cancelled");
        assert_eq!(p["payload_version"], 2);
        assert_eq!(p["log_evidence"], "decoded");
        assert_eq!(p["review_outcome"], "cancelled");
        assert_eq!(p["listing"], pk(2).to_string()); // account index 1
        assert_eq!(p["cluster"], "devnet");
        assert_eq!(p["instruction_path"], "0");
        assert_eq!(p["tx_signature"], sig().to_string());
        assert_eq!(p["program"], "marketplace");
    }

    #[test]
    fn a_logless_silent_verdict_is_recorded_with_a_null_silent_seat() {
        // Truncated log messages are a normal Solana outcome. The cancellation is
        // chain-guaranteed, so it is still announced with `status: "Cancelled"`; only
        // `silent_lawyer` degrades to null, marked by `log_evidence: "missing"` (ADR-52).
        let m = map(
            MarketplaceInstruction::ResolveSilentVerdict(ResolveSilentVerdict { listing_id: 7 }),
            2,
        );
        assert_eq!(m.webhook_events.len(), 1, "the trigger is still recorded");
        let p = &m.webhook_events[0].payload;
        assert_eq!(
            sorted_keys(p),
            SILENT_VERDICT_RESOLVED_KEYS,
            "the pinned v2 silent_verdict_resolved key set"
        );
        assert_eq!(p["silent_lawyer"], serde_json::Value::Null);
        assert_eq!(p["status"], "Cancelled");
        assert_eq!(p["payload_version"], 2);
        assert_eq!(p["log_evidence"], "missing");
        assert_eq!(p["review_outcome"], "cancelled");
    }

    #[test]
    fn a_logless_document_verdict_is_recorded_as_unresolved_not_guessed() {
        let m = map(confirm_documents(), 3);
        let p = &m.webhook_events[0].payload;
        assert_eq!(p["log_evidence"], "missing");
        assert_eq!(p["status"], serde_json::Value::Null);
        assert_eq!(p["review_outcome"], "unresolved");
        // The instruction's own facts still ride along, so the consumer can record the verdict.
        assert_eq!(p["lawyer"], pk(1).to_string());
        assert_eq!(p["approve"], serde_json::json!(true));
    }

    #[test]
    fn an_unrelated_log_line_in_the_same_instruction_is_never_a_false_decode() {
        // Three shapes of noise inside the mapped instruction's own logs: an Anchor
        // "Instruction:" log line, a nested CPI frame that emitted the event one path DEEPER
        // than this instruction, and a sibling event type's payload. None may decode into a
        // status; the row says "missing" or "malformed" and carries a null status.
        let noise: [(&str, Vec<String>); 3] = [
            (
                "an anchor instruction log line",
                vec![
                    format!("Program {PROGRAM_ID} invoke [1]"),
                    "Program log: Instruction: LawyerConfirmDocuments.".to_string(),
                    "Program log: 4 times the fee, 12 shares.".to_string(),
                    format!("Program {PROGRAM_ID} success"),
                ],
            ),
            (
                "an event emitted one nested frame deeper",
                vec![
                    format!("Program {PROGRAM_ID} invoke [1]"),
                    format!("Program {PROGRAM_ID} invoke [2]"),
                    format!("Program data: {LEGAL_LOG}"),
                    format!("Program {PROGRAM_ID} success"),
                    format!("Program {PROGRAM_ID} success"),
                ],
            ),
            (
                "another event type's payload",
                vec![
                    format!("Program {PROGRAM_ID} invoke [1]"),
                    // SilentVerdictResolved's emit, not DocumentsConfirmed's.
                    "Program data: Js+sjGC2z3MHAAAAAAAAABUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUV".to_string(),
                    format!("Program {PROGRAM_ID} success"),
                ],
            ),
        ];
        for (label, logs) in noise {
            let m = map_with_logs(confirm_documents(), 3, &[0], logs);
            let p = &m.webhook_events[0].payload;
            assert_ne!(p["log_evidence"], "decoded", "{label} must not decode");
            assert!(
                matches!(
                    p["log_evidence"].as_str(),
                    Some("missing") | Some("malformed")
                ),
                "{label}: unexpected log_evidence {:?}",
                p["log_evidence"]
            );
            assert_eq!(p["status"], serde_json::Value::Null, "{label}");
            assert_eq!(p["review_outcome"], "unresolved", "{label}");
            assert_eq!(p["listing_id"], serde_json::json!(7), "{label}");
        }
    }

    #[test]
    fn a_wrong_discriminator_or_truncated_event_body_is_malformed_evidence() {
        // Both shapes reach the generated decoder and fail there, which is exactly what
        // `log_evidence: "malformed"` means: logs existed, none was this event.
        for (label, event_data) in [
            (
                "a wrong discriminator",
                // Every field intact, but the discriminator is [0; 8] instead of
                // DocumentsConfirmed's.
                "AAAAAAAAAAAHAAAAAAAAAAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJAw==",
            ),
            (
                "a truncated body",
                // DocumentsConfirmed's discriminator, then the fields cut off mid-wallet.
                "V1Jwgqou52IHAAAAAAAAAAEBAQE=",
            ),
            (
                "a short junk line",
                "AAECAwQFBgc=",
            ),
        ] {
            let m = map_with_event_log(confirm_documents(), 3, event_data);
            let p = &m.webhook_events[0].payload;
            assert_eq!(p["log_evidence"], "malformed", "{label}");
            assert_eq!(p["status"], serde_json::Value::Null, "{label}");
            assert_eq!(p["review_outcome"], "unresolved", "{label}");
        }
    }

    #[test]
    fn document_event_ids_and_log_decoding_are_scoped_to_each_instruction_path() {
        for path in [vec![0], vec![1]] {
            let m = map_with_event_log_at_path(confirm_documents(), 3, LEGAL_LOG, &path);
            let ev = &m.webhook_events[0];
            let path = path.iter().map(u8::to_string).collect::<Vec<_>>().join(".");
            assert_eq!(ev.event_id, format!("documents_confirmed:{}:{path}", sig()));
            assert_eq!(ev.payload["instruction_path"], path);
            assert_eq!(ev.payload["log_evidence"], "decoded");
            assert_eq!(ev.payload["status"], "Legal");
        }
    }

    #[test]
    fn nested_document_event_uses_its_own_log_and_full_instruction_path() {
        let m = map_with_event_log_at_path(confirm_documents(), 3, SOLD_OUT_LOG, &[0, 0]);
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_id, format!("documents_confirmed:{}:0.0", sig()));
        assert_eq!(ev.payload["instruction_path"], "0.0");
        assert_eq!(ev.payload["log_evidence"], "decoded");
        assert_eq!(ev.payload["status"], "SoldOut");
        assert_eq!(ev.payload["review_outcome"], "unresolved");
    }

    #[test]
    fn execute_deal_emits_the_deal_executed_event() {
        // ADR-36: accounts 2/3 are the listing + property PDAs, 5/6 the two lawyer registry
        // PDAs whose holders lose bucket access.
        let m = map(
            MarketplaceInstruction::ExecuteDeal(ExecuteDeal { listing_id: 7 }),
            7,
        );
        assert_eq!(m.instruction.ix_name, "execute_deal");
        assert_eq!(m.webhook_events.len(), 1);
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_type, "deal_executed");
        assert_eq!(ev.event_id, format!("deal_executed:{}:0", sig()));
        let p = &ev.payload;
        assert_eq!(p["event"], "deal_executed");
        assert_eq!(p["listing_id"], serde_json::json!(7));
        assert_eq!(p["property_id"], serde_json::json!(7));
        assert_eq!(p["cluster"], "devnet");
        assert_eq!(p["status"], "Finalized");
        assert_eq!(p["listing"], pk(3).to_string()); // account index 2
        assert_eq!(p["property"], pk(4).to_string()); // account index 3
        assert_eq!(p["developer_lawyer_registry"], pk(6).to_string()); // index 5
        assert_eq!(p["spv_lawyer_registry"], pk(7).to_string()); // index 6
        assert_eq!(p["tx_signature"], sig().to_string());
        assert_eq!(p["program"], "marketplace");
    }

    #[test]
    fn secondary_market_purchases_emit_no_bucket_membership_events() {
        let relisted = map(
            MarketplaceInstruction::BuyRelistedShares(BuyRelistedShares {
                asset_id: 9,
                id: 3,
                amount: 2,
                max_total_cost: 100,
            }),
            8,
        );
        let offer = map(
            MarketplaceInstruction::AcceptOffer(AcceptOffer { id: 3, nonce: 1 }),
            11,
        );
        assert!(relisted.webhook_events.is_empty());
        assert!(offer.webhook_events.is_empty());
    }

    #[test]
    fn send_property_shares_emits_only_the_shares_transferred_notification() {
        let m = map(
            MarketplaceInstruction::SendPropertyShares(SendPropertyShares {
                asset_id: 9,
                amount: 2,
            }),
            7,
        );
        assert_eq!(m.instruction.ix_name, "send_property_shares");
        assert_eq!(m.webhook_events.len(), 1);
        let ev = &m.webhook_events[0];
        assert_eq!(ev.event_type, "shares_transferred");
        assert_eq!(ev.event_id, format!("shares_transferred:{}:0", sig()));
        assert_eq!(ev.slot, SLOT as i64);
        assert_eq!(ev.tx_signature, sig().to_string());
        assert_eq!(ev.block_time, block_time());
        let p = &ev.payload;
        assert_eq!(p["event"], "shares_transferred");
        assert_eq!(p["asset_id"], serde_json::json!(9));
        assert_eq!(p["amount"], serde_json::json!(2));
        assert_eq!(p["sender"], pk(1).to_string());
        assert_eq!(p["receiver"], pk(4).to_string());
        assert_eq!(p["slot"], serde_json::json!(SLOT as i64));
        assert_eq!(p["tx_signature"], sig().to_string());
        assert_eq!(p["block_time"], block_time().to_rfc3339());
        assert_eq!(p["program"], "marketplace");
    }

    #[test]
    fn send_property_shares_needs_the_carried_accounts() {
        let result = map_instruction(
            &metadata(),
            &decoded_for(
                PROGRAM_ID,
                MarketplaceInstruction::SendPropertyShares(SendPropertyShares {
                    asset_id: 9,
                    amount: 1,
                }),
                &pks(3),
            ),
            block_time(),
        );
        assert!(
            result.is_err(),
            "a truncated account list must fail the mapping"
        );
    }

    #[test]
    fn primary_market_holder_changes_emit_no_bucket_membership_events() {
        for (name, data, accounts) in [
            (
                "reserve_shares",
                MarketplaceInstruction::ReserveShares(ReserveShares {
                    listing_id: 7,
                    amount: 5,
                    max_total_cost: 100,
                }),
                8,
            ),
            (
                "buy_property_shares",
                MarketplaceInstruction::BuyPropertyShares(BuyPropertyShares {
                    listing_id: 7,
                    amount: 5,
                    max_total_cost: 100,
                }),
                8,
            ),
            (
                "unreserve_shares",
                MarketplaceInstruction::UnreserveShares(UnreserveShares { listing_id: 7 }),
                4,
            ),
            (
                "release_reservation",
                MarketplaceInstruction::ReleaseReservation(ReleaseReservation {
                    listing_id: 7,
                    investor: pk(9),
                }),
                6,
            ),
            (
                "close_cancelled_position",
                MarketplaceInstruction::CloseCancelledPosition(CloseCancelledPosition {
                    listing_id: 7,
                    investor: pk(9),
                }),
                5,
            ),
            (
                "withdraw_expired",
                MarketplaceInstruction::WithdrawExpired(WithdrawExpired { listing_id: 7 }),
                7,
            ),
            (
                "withdraw_cancelled",
                MarketplaceInstruction::WithdrawCancelled(WithdrawCancelled { listing_id: 7 }),
                7,
            ),
            (
                "withdraw_legal_process_expired",
                MarketplaceInstruction::WithdrawLegalProcessExpired(WithdrawLegalProcessExpired {
                    listing_id: 7,
                }),
                7,
            ),
        ] {
            let m = map(data, accounts);
            assert!(m.webhook_events.is_empty(), "{name}");
        }
    }

    #[test]
    fn the_other_marketplace_instructions_emit_no_webhook_events() {
        // Webhooks fire ONLY on the producer instructions (asset registration, claim start,
        // elections, deal execution, the transfers, holder changes); every other instruction
        // leaves the vector empty (regions/realxhub always do).
        for (name, data, n) in [
            (
                "cancel_offer",
                MarketplaceInstruction::CancelOffer(CancelOffer {}),
                11,
            ),
            (
                "make_offer",
                MarketplaceInstruction::MakeOffer(MakeOffer {
                    id: 1,
                    amount: 2,
                    share_price: 10,
                }),
                13,
            ),
        ] {
            let m = map(data, n);
            assert!(m.webhook_events.is_empty(), "{name}");
        }
    }
}

mod realxhub {
    //! Realxhub (fractional hub shares, fifth program onboarded — ADR-30): two instruction
    //! shapes touch the close logic — `delist_shares` unconditionally closes the seller's
    //! listing (position 2) and `buy_shares` closes it *only if the purchase empties it*
    //! (the IfEmptied arm). Everything else is a plain state upsert and must produce no
    //! closes, no actions, no webhook events. The decoder is exercised end-to-end through
    //! the same `decoded_for` fixture every sibling program uses (ADR-03).
    use super::*;
    use crate::mapping::realxhub::map_instruction;
    use carbon_realxhub_decoder::instructions::{
        BuyShares, ClaimIncome, CreateHub, DelistShares, Faucet, Initialize, ListShares,
        RealxhubInstruction, RecordSale,
    };
    use carbon_realxhub_decoder::types::HubParams;
    use carbon_realxhub_decoder::PROGRAM_ID;

    fn map(data: RealxhubInstruction, n_accounts: usize) -> MappedInstruction {
        map_instruction(
            &metadata(),
            &decoded_for(PROGRAM_ID, data, &pks(n_accounts)),
            block_time(),
        )
        .expect("mapping must succeed")
        .expect("this variant must produce rows")
    }

    /// `delist_shares` [seller, holding, listing] → unconditional close of the listing at
    /// position 2 (the program zeroes the amount; the batcher closes when the post-mapping
    /// balance hits zero — same ruling as the marketplace's `delist_property_shares`).
    #[test]
    fn delist_shares_always_closes_the_listing() {
        let m = map(
            RealxhubInstruction::DelistShares(DelistShares { hub_id: 1 }),
            3,
        );
        assert_eq!(m.instruction.ix_name, "delist_shares");
        expect_close(&m, StateTable::RealxhubShareListing, 2);
    }

    /// `buy_shares` [buyer, seller, hub, listing, …] → the seller's listing (position 3)
    /// closes only if the purchase empties it, carrying the bought amount for the
    /// zero-balance check.
    #[test]
    fn buy_shares_closes_the_listing_if_emptied() {
        let m = map(
            RealxhubInstruction::BuyShares(BuyShares {
                hub_id: 1,
                amount: 25,
                max_cost: 1_000_000_000,
            }),
            15,
        );
        assert_eq!(m.instruction.ix_name, "buy_shares");
        assert_eq!(
            m.closes,
            vec![PendingClose::RealxhubShareListingIfEmptied {
                pubkey: pk(4).to_bytes().to_vec(),
                bought_amount: 25,
                slot: SLOT as i64,
            }]
        );
    }

    /// A short account list on a closing instruction is a loud error, never a silent skip —
    /// the same contract as the marketplace's `close_dead_listing` guard.
    #[test]
    fn a_short_account_list_on_delist_shares_is_a_loud_error() {
        let err = map_instruction(
            &metadata(),
            &decoded_for(
                PROGRAM_ID,
                RealxhubInstruction::DelistShares(DelistShares { hub_id: 1 }),
                &pks(2),
            ),
            block_time(),
        )
        .expect_err("must not silently skip the close");
        assert_eq!(err.reason(), "missing_account");
    }

    /// The state-upsert instructions (faucet, record_sale, claim_income, initialize,
    /// list_shares, create_hub) land rows in the state tables and touch nothing that can
    /// be closed.
    #[test]
    fn the_state_upsert_instructions_produce_no_closes() {
        for (name, data, n_accounts) in [
            ("faucet", RealxhubInstruction::Faucet(Faucet {}), 8),
            (
                "record_sale",
                RealxhubInstruction::RecordSale(RecordSale {
                    hub_id: 1,
                    amount: 1_000,
                }),
                15,
            ),
            (
                "claim_income",
                RealxhubInstruction::ClaimIncome(ClaimIncome { hub_id: 1 }),
                9,
            ),
            (
                "initialize",
                RealxhubInstruction::Initialize(Initialize {}),
                7,
            ),
            (
                "list_shares",
                RealxhubInstruction::ListShares(ListShares {
                    hub_id: 1,
                    amount: 10,
                    price: 1_000_000,
                }),
                4,
            ),
            (
                "create_hub",
                RealxhubInstruction::CreateHub(CreateHub {
                    params: HubParams {
                        name: "spv-hub-1".into(),
                        per_wallet_cap: 10,
                        supplier: pk(1),
                        operators: pk(2),
                        protocol: pk(3),
                    },
                }),
                12,
            ),
        ] {
            let m = map(data, n_accounts);
            assert_eq!(m.instruction.program_id, PROGRAM_ID.to_bytes().to_vec());
            assert_eq!(m.instruction.ix_name, name);
            assert!(
                m.action.is_none(),
                "{name}: realxhub has no webhook actions"
            );
            assert!(m.webhook_events.is_empty(), "{name}: no webhook events");
            assert!(m.closes.is_empty(), "{name}: must not close anything");
        }
    }
}

/// One decode-through test per sibling decoder would need real discriminator bytes; the
/// whitelist's `a_real_borsh_encoded_instruction_decodes_and_maps` already exercises the
/// decoder -> mapper seam, and the sibling decoders are generated by the same tool from the
/// same IDL format. The devnet integration path (snapshot/backfill against the live chain)
/// covers the rest.
#[allow(dead_code)]
fn _coverage_note<T>(_: DecodedInstruction<T>) {}
