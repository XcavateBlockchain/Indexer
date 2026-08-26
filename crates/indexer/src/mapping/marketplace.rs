//! The `marketplace` program mapping: one `program_instructions` row per instruction (no
//! action log -- see the module docs on [`super`]), plus PendingCloses read off the on-chain
//! source's `close =` constraints and runtime `close()` calls. Sixteen closing
//! instructions, twenty close operations:
//!
//! | instruction | closes | at index |
//! |---|---|---|
//! | `unregister_lawyer` | Lawyer | 3 |
//! | `release_reservation` | InvestorPosition | 4 |
//! | `close_reservation` | Reservation | 3 |
//! | `close_cancelled_position` | InvestorPosition | 4 |
//! | `close_candidacy` | LawyerCandidacy | 3 |
//! | `unlock_voting_shares` | LawyerVote | 4 |
//! | `close_dead_listing` | Listing AND PropertyAsset | 4, 5 |
//! | `withdraw_expired` | InvestorPosition AND ShareHolding | 5, 6 |
//! | `withdraw_legal_process_expired` | InvestorPosition AND ShareHolding | 5, 6 |
//! | `withdraw_cancelled` | InvestorPosition AND ShareHolding | 5, 6 |
//! | `accept_offer` | Offer AND ShareListing (CONDITIONAL: emptied) | 10, 6 |
//! | `reject_offer` | Offer | 4 |
//! | `cancel_offer` | Offer | 2 |
//! | `buy_relisted_shares` | ShareListing (CONDITIONAL: emptied) | 6 |
//! | `delist_shares` | ShareListing | 2 |
//! | `close_share_holding` | ShareHolding | 4 |
//!
//! Note InvestorPosition is closed at index 4 by two instructions and index 5 by three
//! others -- close positions are per-instruction facts. `close_case` closes NOTHING despite
//! its name; `close_dead_listing` additionally closes token accounts/mint via CPI, which are
//! not state tables and need no PendingClose. Its optional accounts sit at indices 6 and 9,
//! AFTER both closed accounts, so the close indices are stable.
//!
//! The secondary market brought the program's first runtime closes: `accept_offer` closes
//! its Offer unconditionally at the END of the handler (still a close on every successful
//! transaction, so an ordinary [`PendingClose::Account`]), while the ShareListing in
//! `accept_offer` / `buy_relisted_shares` closes only when the sale emptied it -- a
//! condition on account state the pure mapper cannot evaluate, so those two emit the
//! conditional ops decided by the batcher's SQL (`db::marketplace`). `make_offer` /
//! `relist_shares` / `send_property_shares` close nothing.

use carbon_core::account::{AccountDecoder, DecodedAccount};
use carbon_core::instruction::{DecodedInstruction, InstructionMetadata};
use carbon_marketplace_decoder::accounts::MarketplaceAccount;
use carbon_marketplace_decoder::instructions::MarketplaceInstruction;
use carbon_marketplace_decoder::types::{
    DocumentStatus as ChainDocumentStatus, LawyerAssignment as ChainLawyerAssignment,
    ListingStatus as ChainListingStatus,
};
use carbon_marketplace_decoder::{MarketplaceDecoder, PROGRAM_ID};
use chrono::{DateTime, Utc};
use solana_account::Account;
use solana_pubkey::Pubkey;

use super::{
    account_bytes_at, close_at, instruction_row, ix_context, MappedInstruction, MappingError,
    PendingClose, ProgramMapper, WebhookEvent,
};
use crate::batcher::WriteOp;
use crate::db::close::StateTable;
use crate::db::marketplace::{
    DocumentStatus, InvestorPositionRow, LawyerAssignmentCols, LawyerCandidacyRow, LawyerRow,
    LawyerVoteRow, ListingRow, ListingStatus, MarketplaceAccountRow, MarketplaceConfigRow,
    OfferRow, PropertyAssetRow, ReservationRow, ShareHoldingRow, ShareListingRow,
};

/// The marketplace program's [`ProgramMapper`] instantiation.
pub struct Marketplace;

impl ProgramMapper for Marketplace {
    type Ix = MarketplaceInstruction;
    type Acc = MarketplaceAccount;
    const NAME: &'static str = "marketplace";

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
pub fn ix_name(ix: &MarketplaceInstruction) -> &'static str {
    match ix {
        MarketplaceInstruction::AcceptAuthority(_) => "accept_authority",
        MarketplaceInstruction::AcceptOffer(_) => "accept_offer",
        MarketplaceInstruction::AssignDeveloperLawyer(_) => "assign_developer_lawyer",
        MarketplaceInstruction::BuyPropertyShares(_) => "buy_property_shares",
        MarketplaceInstruction::BuyRelistedShares(_) => "buy_relisted_shares",
        MarketplaceInstruction::CancelOffer(_) => "cancel_offer",
        MarketplaceInstruction::ClaimShares(_) => "claim_shares",
        MarketplaceInstruction::ClaimSpvCase(_) => "claim_spv_case",
        MarketplaceInstruction::CloseCancelledPosition(_) => "close_cancelled_position",
        MarketplaceInstruction::CloseCandidacy(_) => "close_candidacy",
        MarketplaceInstruction::CloseCase(_) => "close_case",
        MarketplaceInstruction::CloseDeadListing(_) => "close_dead_listing",
        MarketplaceInstruction::CloseReservation(_) => "close_reservation",
        MarketplaceInstruction::CloseShareHolding(_) => "close_share_holding",
        MarketplaceInstruction::CreateSpv(_) => "create_spv",
        MarketplaceInstruction::DelistShares(_) => "delist_shares",
        MarketplaceInstruction::ExecuteDeal(_) => "execute_deal",
        MarketplaceInstruction::FinalizeSpvElection(_) => "finalize_spv_election",
        MarketplaceInstruction::InitializeConfig(_) => "initialize_config",
        MarketplaceInstruction::InitPropertyAssets(_) => "init_property_assets",
        MarketplaceInstruction::LawyerConfirmDocuments(_) => "lawyer_confirm_documents",
        MarketplaceInstruction::ListProperty(_) => "list_property",
        MarketplaceInstruction::LockShares(_) => "lock_shares",
        MarketplaceInstruction::MakeOffer(_) => "make_offer",
        MarketplaceInstruction::RegisterLawyer(_) => "register_lawyer",
        MarketplaceInstruction::RejectOffer(_) => "reject_offer",
        MarketplaceInstruction::ReleaseReservation(_) => "release_reservation",
        MarketplaceInstruction::RelistShares(_) => "relist_shares",
        MarketplaceInstruction::ReserveShares(_) => "reserve_shares",
        MarketplaceInstruction::ResignFromCase(_) => "resign_from_case",
        MarketplaceInstruction::ResolveSilentVerdict(_) => "resolve_silent_verdict",
        MarketplaceInstruction::SendPropertyShares(_) => "send_property_shares",
        MarketplaceInstruction::SettleCancelledFees(_) => "settle_cancelled_fees",
        MarketplaceInstruction::UnlockShares(_) => "unlock_shares",
        MarketplaceInstruction::UnlockVotingShares(_) => "unlock_voting_shares",
        MarketplaceInstruction::UnregisterLawyer(_) => "unregister_lawyer",
        MarketplaceInstruction::UnreserveShares(_) => "unreserve_shares",
        MarketplaceInstruction::UpdateAuthority(_) => "update_authority",
        MarketplaceInstruction::UpdateConfig(_) => "update_config",
        MarketplaceInstruction::UpgradeObject(_) => "upgrade_object",
        MarketplaceInstruction::VoteOnSpvLawyer(_) => "vote_on_spv_lawyer",
        MarketplaceInstruction::WithdrawCancelled(_) => "withdraw_cancelled",
        MarketplaceInstruction::WithdrawDepositUnsold(_) => "withdraw_deposit_unsold",
        MarketplaceInstruction::WithdrawExpired(_) => "withdraw_expired",
        MarketplaceInstruction::WithdrawLegalProcessExpired(_) => "withdraw_legal_process_expired",
        MarketplaceInstruction::CpiEvent(_) => "cpi_event",
    }
}

/// Map one decoded marketplace instruction. `Ok(None)` only for the decoder's synthetic
/// `CpiEvent` variant (this program emits log-based `emit!`, never `emit_cpi!`).
pub fn map_instruction(
    metadata: &InstructionMetadata,
    decoded: &DecodedInstruction<MarketplaceInstruction>,
    block_time: DateTime<Utc>,
) -> Result<Option<MappedInstruction>, MappingError> {
    let name = ix_name(&decoded.data);

    if matches!(decoded.data, MarketplaceInstruction::CpiEvent(_)) {
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
        MarketplaceInstruction::UnregisterLawyer(_) => {
            vec![close_at(
                accounts,
                3,
                name,
                StateTable::MarketplaceLawyer,
                slot,
            )?]
        }
        MarketplaceInstruction::ReleaseReservation(_) => vec![close_at(
            accounts,
            4,
            name,
            StateTable::MarketplaceInvestorPosition,
            slot,
        )?],
        MarketplaceInstruction::CloseReservation(_) => vec![close_at(
            accounts,
            3,
            name,
            StateTable::MarketplaceReservation,
            slot,
        )?],
        MarketplaceInstruction::CloseCancelledPosition(_) => vec![close_at(
            accounts,
            4,
            name,
            StateTable::MarketplaceInvestorPosition,
            slot,
        )?],
        MarketplaceInstruction::CloseCandidacy(_) => vec![close_at(
            accounts,
            3,
            name,
            StateTable::MarketplaceLawyerCandidacy,
            slot,
        )?],
        MarketplaceInstruction::UnlockVotingShares(_) => vec![close_at(
            accounts,
            4,
            name,
            StateTable::MarketplaceLawyerVote,
            slot,
        )?],
        MarketplaceInstruction::CloseDeadListing(_) => vec![
            close_at(accounts, 4, name, StateTable::MarketplaceListing, slot)?,
            close_at(
                accounts,
                5,
                name,
                StateTable::MarketplacePropertyAsset,
                slot,
            )?,
        ],
        MarketplaceInstruction::WithdrawExpired(_)
        | MarketplaceInstruction::WithdrawLegalProcessExpired(_)
        | MarketplaceInstruction::WithdrawCancelled(_) => vec![
            close_at(
                accounts,
                5,
                name,
                StateTable::MarketplaceInvestorPosition,
                slot,
            )?,
            close_at(accounts, 6, name, StateTable::MarketplaceShareHolding, slot)?,
        ],
        MarketplaceInstruction::AcceptOffer(_) => vec![
            // The offer closes at the end of the handler on every success (a runtime
            // close, but unconditional); the share listing only if the sale emptied it --
            // by the OFFER's amount, which the pure mapper cannot read, so the batcher
            // decides against the stored offer row.
            close_at(accounts, 10, name, StateTable::MarketplaceOffer, slot)?,
            PendingClose::ShareListingIfEmptiedByOffer {
                pubkey: account_bytes_at(accounts, 6, name)?,
                offer_pubkey: account_bytes_at(accounts, 10, name)?,
                slot,
            },
        ],
        MarketplaceInstruction::RejectOffer(_) => {
            vec![close_at(
                accounts,
                4,
                name,
                StateTable::MarketplaceOffer,
                slot,
            )?]
        }
        MarketplaceInstruction::CancelOffer(_) => {
            vec![close_at(
                accounts,
                2,
                name,
                StateTable::MarketplaceOffer,
                slot,
            )?]
        }
        MarketplaceInstruction::BuyRelistedShares(args) => {
            // Closed on-chain only when this buy took the listing's last shares; the
            // instruction's own `amount` arg carries how many were bought.
            vec![PendingClose::ShareListingIfEmptied {
                pubkey: account_bytes_at(accounts, 6, name)?,
                bought_amount: args.amount as i64,
                slot,
            }]
        }
        MarketplaceInstruction::DelistShares(_) => vec![close_at(
            accounts,
            2,
            name,
            StateTable::MarketplaceShareListing,
            slot,
        )?],
        MarketplaceInstruction::CloseShareHolding(_) => vec![close_at(
            accounts,
            4,
            name,
            StateTable::MarketplaceShareHolding,
            slot,
        )?],
        _ => vec![],
    };

    // ADR-28: `init_property_assets` is the registration of a property asset -- the moment the
    // `PropertyAsset` PDA (account index 3, seeded `["property", listing_id]`) gets its name +
    // metadata_uri + share mint (index 4). Record a durable, idempotent webhook event
    // (deduped by the asset PDA) that the background loop delivers to `WEBHOOK_URL`.
    let webhook_events = match &decoded.data {
        MarketplaceInstruction::InitPropertyAssets(args) => {
            let property_pubkey = account_bytes_at(accounts, 3, name)?;
            let share_mint = account_bytes_at(accounts, 4, name)?;
            let property_b58 = bs58::encode(&property_pubkey).into_string();
            let share_mint_b58 = bs58::encode(&share_mint).into_string();
            let tx_signature = ctx.tx_signature.clone();
            vec![WebhookEvent {
                event_id: format!("property_asset_registered:{property_b58}"),
                event_type: "property_asset_registered",
                payload: serde_json::json!({
                    "event": "property_asset_registered",
                    "pubkey": &property_b58,
                    "listing_id": args.listing_id,
                    "name": &args.name,
                    "metadata_uri": &args.uri,
                    "share_mint": &share_mint_b58,
                    "slot": slot,
                    "tx_signature": &tx_signature,
                    "block_time": block_time.to_rfc3339(),
                    "program": "marketplace",
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

fn listing_status_from_chain(status: &ChainListingStatus) -> ListingStatus {
    match status {
        ChainListingStatus::PendingAssets => ListingStatus::PendingAssets,
        ChainListingStatus::Listed => ListingStatus::Listed,
        ChainListingStatus::SoldOut => ListingStatus::SoldOut,
        ChainListingStatus::Legal => ListingStatus::Legal,
        ChainListingStatus::Finalized => ListingStatus::Finalized,
        ChainListingStatus::Expired => ListingStatus::Expired,
        ChainListingStatus::Cancelled => ListingStatus::Cancelled,
        ChainListingStatus::Refunding => ListingStatus::Refunding,
    }
}

fn doc_status_from_chain(status: &ChainDocumentStatus) -> DocumentStatus {
    match status {
        ChainDocumentStatus::Pending => DocumentStatus::Pending,
        ChainDocumentStatus::Approved => DocumentStatus::Approved,
        ChainDocumentStatus::Rejected => DocumentStatus::Rejected,
    }
}

fn lawyer_assignment_cols(a: &ChainLawyerAssignment) -> LawyerAssignmentCols {
    LawyerAssignmentCols {
        lawyer: a.lawyer.to_bytes().to_vec(),
        costs: a.costs as i64,
        doc_status: doc_status_from_chain(&a.doc_status),
        documents_hash: a.documents_hash.to_vec(),
    }
}

/// Decoded account -> state-table upsert (same contract as the whitelist's; see
/// [`super::whitelist::account_write_op`]). JSONB shapes are constructed here, NOT taken from
/// the decoder's serde output: pubkeys as base58 strings, amounts as JSON numbers.
pub fn account_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    decoded: &DecodedAccount<MarketplaceAccount>,
) -> WriteOp {
    let pubkey = pubkey.to_bytes().to_vec();
    let row = match &decoded.data {
        MarketplaceAccount::Config(c) => MarketplaceAccountRow::Config(MarketplaceConfigRow {
            pubkey,
            slot,
            lamports,
            authority: c.authority.to_bytes().to_vec(),
            pending_authority: c.pending_authority.map(|p| p.to_bytes().to_vec()),
            xcav_mint: c.xcav_mint.to_bytes().to_vec(),
            treasury: c.treasury.to_bytes().to_vec(),
            rent_collector: c.rent_collector.to_bytes().to_vec(),
            accepted_payment_mints: serde_json::Value::Array(
                c.accepted_payment_mints
                    .iter()
                    .map(|m| serde_json::Value::String(m.to_string()))
                    .collect(),
            ),
            listing_deposit: c.listing_deposit as i64,
            lawyer_deposit: c.lawyer_deposit as i64,
            min_property_shares: c.min_property_shares as i64,
            max_property_shares: c.max_property_shares as i64,
            marketplace_fee_bps: c.marketplace_fee_bps as i32,
            investor_fee_bps: c.investor_fee_bps as i32,
            max_ownership_bps: c.max_ownership_bps as i32,
            claiming_time: c.claiming_time,
            legal_process_time: c.legal_process_time,
            lawyer_voting_time: c.lawyer_voting_time,
            min_voting_quorum_bps: c.min_voting_quorum_bps as i32,
            next_listing_id: c.next_listing_id as i64,
            next_share_listing_id: c.next_share_listing_id as i64,
            bump: c.bump as i16,
        }),
        MarketplaceAccount::InvestorPosition(p) => {
            MarketplaceAccountRow::InvestorPosition(InvestorPositionRow {
                pubkey,
                slot,
                lamports,
                listing_id: p.listing_id as i64,
                investor: p.investor.to_bytes().to_vec(),
                payment_mint: p.payment_mint.to_bytes().to_vec(),
                payment_account: p.payment_account.to_bytes().to_vec(),
                share_amount: p.share_amount as i64,
                reserved_share_amount: p.reserved_share_amount as i64,
                paid_funds: p.paid_funds as i64,
                paid_tax: p.paid_tax as i64,
                paid_fee: p.paid_fee as i64,
                reserved_funds: p.reserved_funds as i64,
                reserved_tax: p.reserved_tax as i64,
                reserved_fee: p.reserved_fee as i64,
                cancelled: p.cancelled,
                bump: p.bump as i16,
            })
        }
        MarketplaceAccount::Lawyer(l) => MarketplaceAccountRow::Lawyer(LawyerRow {
            pubkey,
            slot,
            lamports,
            lawyer: l.lawyer.to_bytes().to_vec(),
            region_id: l.region_id as i32,
            deposit: l.deposit as i64,
            active_cases: l.active_cases as i64,
            bump: l.bump as i16,
        }),
        MarketplaceAccount::LawyerCandidacy(c) => {
            MarketplaceAccountRow::LawyerCandidacy(LawyerCandidacyRow {
                pubkey,
                slot,
                lamports,
                listing_id: c.listing_id as i64,
                round: c.round as i64,
                lawyer: c.lawyer.to_bytes().to_vec(),
                costs: c.costs as i64,
                vote_power: c.vote_power as i64,
                rent_payer: c.rent_payer.to_bytes().to_vec(),
                bump: c.bump as i16,
            })
        }
        MarketplaceAccount::LawyerVote(v) => MarketplaceAccountRow::LawyerVote(LawyerVoteRow {
            pubkey,
            slot,
            lamports,
            listing_id: v.listing_id as i64,
            round: v.round as i64,
            voter: v.voter.to_bytes().to_vec(),
            choice: v.choice.to_bytes().to_vec(),
            power: v.power as i64,
            rent_payer: v.rent_payer.to_bytes().to_vec(),
            bump: v.bump as i16,
        }),
        MarketplaceAccount::Listing(l) => MarketplaceAccountRow::Listing(Box::new(ListingRow {
            pubkey,
            slot,
            lamports,
            listing_id: l.listing_id as i64,
            developer: l.developer.to_bytes().to_vec(),
            asset_id: l.asset_id as i64,
            share_price: l.share_price as i64,
            listed_share_amount: l.listed_share_amount as i64,
            sold_share_amount: l.sold_share_amount as i64,
            reserved_share_amount: l.reserved_share_amount as i64,
            tax_paid_by_developer: l.tax_paid_by_developer,
            tax_bps: l.tax_bps as i32,
            marketplace_fee_bps: l.marketplace_fee_bps as i32,
            investor_fee_bps: l.investor_fee_bps as i32,
            max_ownership_bps: l.max_ownership_bps as i32,
            listing_expiry: l.listing_expiry,
            claiming_time: l.claiming_time,
            claim_deadline: l.claim_deadline,
            legal_process_time: l.legal_process_time,
            lawyer_voting_time: l.lawyer_voting_time,
            min_voting_quorum_bps: l.min_voting_quorum_bps as i32,
            position_count: l.position_count as i64,
            legal_deadline: l.legal_deadline,
            deposit: l.deposit as i64,
            developer_lawyer: lawyer_assignment_cols(&l.developer_lawyer),
            spv_lawyer: lawyer_assignment_cols(&l.spv_lawyer),
            second_attempt: l.second_attempt,
            developer_engaged: l.developer_engaged,
            spv_costs_due: l.spv_costs_due as i64,
            spv_costs_payee: l.spv_costs_payee.to_bytes().to_vec(),
            collected_fee_quote: l.collected_fee_quote as i64,
            collected: serde_json::Value::Array(
                l.collected
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "mint": c.mint.to_string(),
                            "funds": c.funds,
                            "fee": c.fee,
                            "tax": c.tax,
                        })
                    })
                    .collect(),
            ),
            spv_election_expiry: l.spv_election.expiry,
            spv_election_candidate_count: l.spv_election.candidate_count as i64,
            spv_election_round: l.spv_election.round as i64,
            status: listing_status_from_chain(&l.status),
            bump: l.bump as i16,
        })),
        MarketplaceAccount::PropertyAsset(p) => {
            MarketplaceAccountRow::PropertyAsset(PropertyAssetRow {
                pubkey,
                slot,
                lamports,
                asset_id: p.asset_id as i64,
                name: p.name.clone(),
                metadata_uri: p.metadata_uri.clone(),
                share_mint: p.share_mint.to_bytes().to_vec(),
                region_id: p.region_id as i32,
                location: p.location.clone(),
                share_amount: p.share_amount as i64,
                spv_created: p.spv_created,
                finalized: p.finalized,
                holder_count: p.holder_count as i64,
                bump: p.bump as i16,
            })
        }
        MarketplaceAccount::Reservation(r) => MarketplaceAccountRow::Reservation(ReservationRow {
            pubkey,
            slot,
            lamports,
            token_account: r.token_account.to_bytes().to_vec(),
            amount: r.amount as i64,
            bump: r.bump as i16,
        }),
        MarketplaceAccount::ShareHolding(h) => {
            MarketplaceAccountRow::ShareHolding(ShareHoldingRow {
                pubkey,
                slot,
                lamports,
                asset_id: h.asset_id as i64,
                owner: h.owner.to_bytes().to_vec(),
                amount: h.amount as i64,
                // The [u32; 4] lock array, one counter per LockReason in borsh order (the
                // migration-0012 columns are named after the variants).
                lock_lawyer_election: h.locks[0] as i64,
                lock_agent_election: h.locks[1] as i64,
                lock_proposal: h.locks[2] as i64,
                lock_challenge: h.locks[3] as i64,
                listed: h.listed as i64,
                bump: h.bump as i16,
            })
        }
        MarketplaceAccount::ShareListing(l) => {
            MarketplaceAccountRow::ShareListing(ShareListingRow {
                pubkey,
                slot,
                lamports,
                id: l.id as i64,
                asset_id: l.asset_id as i64,
                seller: l.seller.to_bytes().to_vec(),
                share_price: l.share_price as i64,
                amount: l.amount as i64,
                fee_bps: l.fee_bps as i32,
                next_offer_nonce: l.next_offer_nonce as i64,
                rent_payer: l.rent_payer.to_bytes().to_vec(),
                bump: l.bump as i16,
            })
        }
        MarketplaceAccount::Offer(o) => MarketplaceAccountRow::Offer(OfferRow {
            pubkey,
            slot,
            lamports,
            listing_id: o.listing_id as i64,
            asset_id: o.asset_id as i64,
            offeror: o.offeror.to_bytes().to_vec(),
            share_price: o.share_price as i64,
            amount: o.amount as i64,
            payment_mint: o.payment_mint.to_bytes().to_vec(),
            held: o.held as i64,
            nonce: o.nonce as i64,
            rent_payer: o.rent_payer.to_bytes().to_vec(),
            bump: o.bump as i16,
        }),
    };
    WriteOp::UpsertMarketplaceAccount(row)
}

/// Decode one `getProgramAccounts` result with this program's decoder and map it exactly like
/// a live account update. `None` = owned by the program but undecodable (IDL drift).
pub fn snapshot_write_op(
    pubkey: Pubkey,
    slot: i64,
    lamports: i64,
    account: &Account,
) -> Option<WriteOp> {
    let decoded = MarketplaceDecoder.decode_account(account)?;
    Some(account_write_op(pubkey, slot, lamports, &decoded))
}
