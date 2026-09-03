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
        ChallengeAgent, CloseAgentCandidacy, CloseIncomeCheckpoint, FinalizeChallenge,
        FinalizeProposal, FinalizeResignation, PropertyInstruction, Propose, RemoveLettingAgent,
        UnlockAgentVotes, UnlockChallengeVotes, UnlockProposalVotes, VoteOnAgent,
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
}

// --- marketplace ----------------------------------------------------------------------------

mod marketplace {
    use super::*;
    use crate::mapping::marketplace::map_instruction;
    use carbon_marketplace_decoder::instructions::{
        AcceptOffer, BuyPropertyShares, BuyRelistedShares, CancelOffer, CloseCancelledPosition,
        CloseCase, CloseDeadListing, CloseShareHolding, DelistShares, InitPropertyAssets,
        MakeOffer, MarketplaceInstruction, RejectOffer, ReleaseReservation, RelistShares,
        SendPropertyShares, UnregisterLawyer, WithdrawCancelled, WithdrawExpired,
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
        assert_eq!(p["name"], "42 Main Street");
        assert_eq!(p["metadata_uri"], "https://metadata.example/42-main");
        assert_eq!(p["share_mint"], share_mint_b58);
        assert_eq!(p["slot"], serde_json::json!(SLOT as i64));
        assert_eq!(p["tx_signature"], sig().to_string());
        assert_eq!(p["block_time"], block_time().to_rfc3339());
        assert_eq!(p["program"], "marketplace");
    }

    #[test]
    fn the_other_marketplace_instructions_emit_no_webhook_events() {
        // The webhook fires ONLY on asset registration; every other instruction leaves the
        // vector empty (the other three programs always do).
        for (name, data, n) in [
            (
                "cancel_offer",
                MarketplaceInstruction::CancelOffer(CancelOffer {}),
                11,
            ),
            (
                "buy_property_shares",
                MarketplaceInstruction::BuyPropertyShares(BuyPropertyShares {
                    listing_id: 1,
                    amount: 2,
                    max_total_cost: 100,
                }),
                12,
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
