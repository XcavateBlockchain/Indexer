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
        CloseAgentCandidacy, FinalizeResignation, PropertyInstruction, RemoveLettingAgent,
        UnlockAgentVotes, VoteOnAgent,
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
}

// --- marketplace ----------------------------------------------------------------------------

mod marketplace {
    use super::*;
    use crate::mapping::marketplace::map_instruction;
    use carbon_marketplace_decoder::instructions::{
        BuyPropertyShares, CloseCancelledPosition, CloseCase, CloseDeadListing,
        MarketplaceInstruction, ReleaseReservation, UnregisterLawyer, WithdrawCancelled,
        WithdrawExpired, WithdrawLegalProcessExpired,
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
}

/// One decode-through test per sibling decoder would need real discriminator bytes; the
/// whitelist's `a_real_borsh_encoded_instruction_decodes_and_maps` already exercises the
/// decoder -> mapper seam, and the sibling decoders are generated by the same tool from the
/// same IDL format. The devnet integration path (snapshot/backfill against the live chain)
/// covers the rest.
#[allow(dead_code)]
fn _coverage_note<T>(_: DecodedInstruction<T>) {}
