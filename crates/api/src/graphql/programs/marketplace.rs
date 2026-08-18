//! The `marketplace` program's read surface: entity types over
//! `migrations/0009_marketplace_state.sql` and the resolver bodies `QueryRoot` delegates to.
//! See [`super`] for the shared conventions.

use carbon_core::graphql::primitives::I64;
use juniper::{FieldResult, GraphQLObject, ID};

use super::{b58, hex_string, json_string, parse_b58, total_count_i32, utf8_lossy};
use crate::graphql::context::GraphQLContext;
use crate::graphql::enums::{unknown_enum_value, DocumentStatus, ListingStatus};
use crate::guards::{clamp_first, clamp_offset};

/// The marketplace program's Config PDA (singleton). `null` until `initialize_config` has been
/// indexed. `accepted_payment_mints` is the raw JSON of the accepted mint list
/// (`["<base58>", ...]` -- see migration 0009).
#[derive(GraphQLObject, Clone, Debug)]
pub struct MarketplaceConfig {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub authority: String,
    pub pending_authority: Option<String>,
    pub xcav_mint: String,
    pub treasury: String,
    pub rent_collector: String,
    pub accepted_payment_mints: String,
    pub listing_deposit: I64,
    pub lawyer_deposit: I64,
    pub min_property_shares: I64,
    pub max_property_shares: I64,
    pub marketplace_fee_bps: i32,
    pub investor_fee_bps: i32,
    pub max_ownership_bps: i32,
    pub claiming_time: I64,
    pub legal_process_time: I64,
    pub lawyer_voting_time: I64,
    pub min_voting_quorum_bps: i32,
    pub next_listing_id: I64,
}

/// One investor's position in one listing: bought/reserved share counts and the funds, tax and
/// fee amounts paid or reserved for them. Closed and routinely re-created at the same address
/// by the next buy/reserve.
#[derive(GraphQLObject, Clone, Debug)]
pub struct InvestorPosition {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub listing_id: I64,
    pub investor: String,
    pub payment_mint: String,
    pub payment_account: String,
    pub share_amount: I64,
    pub reserved_share_amount: I64,
    pub paid_funds: I64,
    pub paid_tax: I64,
    pub paid_fee: I64,
    pub reserved_funds: I64,
    pub reserved_tax: I64,
    pub reserved_fee: I64,
    pub cancelled: bool,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct InvestorPositionConnection {
    pub nodes: Vec<InvestorPosition>,
    pub total_count: i32,
}

/// A lawyer registered with the marketplace (the registry PDA keyed by the lawyer's wallet).
#[derive(GraphQLObject, Clone, Debug)]
pub struct Lawyer {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub lawyer: String,
    pub region_id: i32,
    pub deposit: I64,
    pub active_cases: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerConnection {
    pub nodes: Vec<Lawyer>,
    pub total_count: i32,
}

/// A lawyer's candidacy for one listing's SPV-lawyer election round.
#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerCandidacy {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub listing_id: I64,
    pub round: I64,
    pub lawyer: String,
    pub costs: I64,
    pub vote_power: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerCandidacyConnection {
    pub nodes: Vec<LawyerCandidacy>,
    pub total_count: i32,
}

/// One voter's vote in a listing's SPV-lawyer election round.
#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerVote {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub listing_id: I64,
    pub round: I64,
    pub voter: String,
    pub choice: String,
    pub power: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct LawyerVoteConnection {
    pub nodes: Vec<LawyerVote>,
    pub total_count: i32,
}

/// A property listing: sale terms, progress counters, the two flattened `LawyerAssignment`s,
/// the flattened `SpvElection` and per-mint collected totals. `collected` is the raw JSON
/// `[{"mint": "<base58>", "funds": N, "fee": N, "tax": N}, ...]` (see migration 0009).
#[derive(GraphQLObject, Clone, Debug)]
pub struct Listing {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub listing_id: I64,
    pub developer: String,
    pub asset_id: I64,
    pub share_price: I64,
    pub listed_share_amount: I64,
    pub sold_share_amount: I64,
    pub reserved_share_amount: I64,
    pub tax_paid_by_developer: bool,
    pub tax_bps: i32,
    pub marketplace_fee_bps: i32,
    pub investor_fee_bps: i32,
    pub max_ownership_bps: i32,
    pub listing_expiry: I64,
    pub claiming_time: I64,
    pub claim_deadline: I64,
    pub legal_process_time: I64,
    pub lawyer_voting_time: I64,
    pub min_voting_quorum_bps: i32,
    pub position_count: I64,
    pub legal_deadline: I64,
    pub deposit: I64,
    pub developer_lawyer: String,
    pub developer_lawyer_costs: I64,
    pub developer_lawyer_doc_status: DocumentStatus,
    pub developer_lawyer_documents_hash: String,
    pub spv_lawyer: String,
    pub spv_lawyer_costs: I64,
    pub spv_lawyer_doc_status: DocumentStatus,
    pub spv_lawyer_documents_hash: String,
    pub second_attempt: bool,
    pub developer_engaged: bool,
    pub spv_costs_due: I64,
    pub spv_costs_payee: String,
    pub collected: String,
    pub spv_election_expiry: I64,
    pub spv_election_candidate_count: I64,
    pub spv_election_round: I64,
    pub status: ListingStatus,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ListingConnection {
    pub nodes: Vec<Listing>,
    pub total_count: i32,
}

/// The tokenised property behind one listing (`asset_id == listing_id` in current source).
/// `core_asset` keeps the on-chain all-zero pubkey verbatim (base58
/// `11111111111111111111111111111111`) -- it is only ever that today.
#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyAsset {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub core_asset: String,
    pub share_mint: String,
    pub region_id: i32,
    pub location: String,
    pub share_amount: I64,
    pub spv_created: bool,
    pub finalized: bool,
    pub holder_count: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyAssetConnection {
    pub nodes: Vec<PropertyAsset>,
    pub total_count: i32,
}

/// A reservation total keyed by the investor's payment token account (shared across listings).
/// Closed at zero and re-created by the next reserve.
#[derive(GraphQLObject, Clone, Debug)]
pub struct Reservation {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub token_account: String,
    pub amount: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ReservationConnection {
    pub nodes: Vec<Reservation>,
    pub total_count: i32,
}

/// One owner's share holding in one property asset.
#[derive(GraphQLObject, Clone, Debug)]
pub struct ShareHolding {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub owner: String,
    pub amount: I64,
    pub locked_amount: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ShareHoldingConnection {
    pub nodes: Vec<ShareHolding>,
    pub total_count: i32,
}

// --- resolver bodies ------------------------------------------------------------------------

pub async fn marketplace_config(
    context: &GraphQLContext,
) -> FieldResult<Option<MarketplaceConfig>> {
    let row = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, authority, pending_authority, xcav_mint,
               treasury, rent_collector, accepted_payment_mints, listing_deposit, lawyer_deposit,
               min_property_shares, max_property_shares, marketplace_fee_bps, investor_fee_bps,
               max_ownership_bps, claiming_time, legal_process_time, lawyer_voting_time,
               min_voting_quorum_bps, next_listing_id
        FROM marketplace_config
        ORDER BY slot DESC
        LIMIT 1
        "#
    )
    .fetch_optional(&context.pool)
    .await?;
    Ok(row.map(|r| MarketplaceConfig {
        id: ID::new(b58(&r.pubkey)),
        slot: I64(r.slot),
        lamports: I64(r.lamports),
        active: r.closed_at_slot.is_none(),
        closed_at_slot: r.closed_at_slot.map(I64),
        authority: b58(&r.authority),
        pending_authority: r.pending_authority.as_deref().map(b58),
        xcav_mint: b58(&r.xcav_mint),
        treasury: b58(&r.treasury),
        rent_collector: b58(&r.rent_collector),
        accepted_payment_mints: json_string(&r.accepted_payment_mints),
        listing_deposit: I64(r.listing_deposit),
        lawyer_deposit: I64(r.lawyer_deposit),
        min_property_shares: I64(r.min_property_shares),
        max_property_shares: I64(r.max_property_shares),
        marketplace_fee_bps: r.marketplace_fee_bps,
        investor_fee_bps: r.investor_fee_bps,
        max_ownership_bps: r.max_ownership_bps,
        claiming_time: I64(r.claiming_time),
        legal_process_time: I64(r.legal_process_time),
        lawyer_voting_time: I64(r.lawyer_voting_time),
        min_voting_quorum_bps: r.min_voting_quorum_bps,
        next_listing_id: I64(r.next_listing_id),
    }))
}

pub async fn investor_positions(
    context: &GraphQLContext,
    listing_id: Option<I64>,
    investor: Option<String>,
    cancelled: Option<bool>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<InvestorPositionConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let listing_id = listing_id.map(|v| v.0);
    let investor = investor
        .as_deref()
        .map(|s| parse_b58("investor", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, listing_id, investor, payment_mint,
               payment_account, share_amount, reserved_share_amount, paid_funds, paid_tax,
               paid_fee, reserved_funds, reserved_tax, reserved_fee, cancelled
        FROM marketplace_investor_position
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR investor = $2)
          AND ($3::bool IS NULL OR cancelled = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $5 OFFSET $6
        "#,
        listing_id,
        investor.as_deref(),
        cancelled,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_investor_position
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR investor = $2)
          AND ($3::bool IS NULL OR cancelled = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        "#,
        listing_id,
        investor.as_deref(),
        cancelled,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(InvestorPositionConnection {
        nodes: rows
            .into_iter()
            .map(|r| InvestorPosition {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                listing_id: I64(r.listing_id),
                investor: b58(&r.investor),
                payment_mint: b58(&r.payment_mint),
                payment_account: b58(&r.payment_account),
                share_amount: I64(r.share_amount),
                reserved_share_amount: I64(r.reserved_share_amount),
                paid_funds: I64(r.paid_funds),
                paid_tax: I64(r.paid_tax),
                paid_fee: I64(r.paid_fee),
                reserved_funds: I64(r.reserved_funds),
                reserved_tax: I64(r.reserved_tax),
                reserved_fee: I64(r.reserved_fee),
                cancelled: r.cancelled,
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn lawyers(
    context: &GraphQLContext,
    lawyer: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<LawyerConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let lawyer = lawyer
        .as_deref()
        .map(|s| parse_b58("lawyer", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, lawyer, region_id, deposit, active_cases
        FROM marketplace_lawyer
        WHERE ($1::bytea IS NULL OR lawyer = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $3 OFFSET $4
        "#,
        lawyer.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_lawyer
        WHERE ($1::bytea IS NULL OR lawyer = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        "#,
        lawyer.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(LawyerConnection {
        nodes: rows
            .into_iter()
            .map(|r| Lawyer {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                lawyer: b58(&r.lawyer),
                region_id: r.region_id,
                deposit: I64(r.deposit),
                active_cases: I64(r.active_cases),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn lawyer_candidacies(
    context: &GraphQLContext,
    listing_id: Option<I64>,
    lawyer: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<LawyerCandidacyConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let listing_id = listing_id.map(|v| v.0);
    let lawyer = lawyer
        .as_deref()
        .map(|s| parse_b58("lawyer", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, listing_id, round, lawyer, costs,
               vote_power, rent_payer
        FROM marketplace_lawyer_candidacy
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR lawyer = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        listing_id,
        lawyer.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_lawyer_candidacy
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR lawyer = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        listing_id,
        lawyer.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(LawyerCandidacyConnection {
        nodes: rows
            .into_iter()
            .map(|r| LawyerCandidacy {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                listing_id: I64(r.listing_id),
                round: I64(r.round),
                lawyer: b58(&r.lawyer),
                costs: I64(r.costs),
                vote_power: I64(r.vote_power),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn lawyer_votes(
    context: &GraphQLContext,
    listing_id: Option<I64>,
    voter: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<LawyerVoteConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let listing_id = listing_id.map(|v| v.0);
    let voter = voter
        .as_deref()
        .map(|s| parse_b58("voter", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, listing_id, round, voter, choice, power,
               rent_payer
        FROM marketplace_lawyer_vote
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR voter = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        listing_id,
        voter.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_lawyer_vote
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR voter = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        listing_id,
        voter.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(LawyerVoteConnection {
        nodes: rows
            .into_iter()
            .map(|r| LawyerVote {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                listing_id: I64(r.listing_id),
                round: I64(r.round),
                voter: b58(&r.voter),
                choice: b58(&r.choice),
                power: I64(r.power),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn listings(
    context: &GraphQLContext,
    listing_id: Option<I64>,
    developer: Option<String>,
    status: Option<ListingStatus>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ListingConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let listing_id = listing_id.map(|v| v.0);
    let developer = developer
        .as_deref()
        .map(|s| parse_b58("developer", s))
        .transpose()?;
    let status = status.map(|s| s.as_db_str());

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, listing_id, developer, asset_id,
               share_price, listed_share_amount, sold_share_amount, reserved_share_amount,
               tax_paid_by_developer, tax_bps, marketplace_fee_bps, investor_fee_bps,
               max_ownership_bps, listing_expiry, claiming_time, claim_deadline,
               legal_process_time, lawyer_voting_time, min_voting_quorum_bps, position_count,
               legal_deadline, deposit, developer_lawyer, developer_lawyer_costs,
               developer_lawyer_doc_status, developer_lawyer_documents_hash, spv_lawyer,
               spv_lawyer_costs, spv_lawyer_doc_status, spv_lawyer_documents_hash,
               second_attempt, developer_engaged, spv_costs_due, spv_costs_payee, collected,
               spv_election_expiry, spv_election_candidate_count, spv_election_round, status
        FROM marketplace_listing
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR developer = $2)
          AND ($3::text IS NULL OR status = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $5 OFFSET $6
        "#,
        listing_id,
        developer.as_deref(),
        status,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_listing
        WHERE ($1::bigint IS NULL OR listing_id = $1)
          AND ($2::bytea IS NULL OR developer = $2)
          AND ($3::text IS NULL OR status = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        "#,
        listing_id,
        developer.as_deref(),
        status,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    let nodes = rows
        .into_iter()
        .map(|r| {
            let developer_lawyer_doc_status =
                DocumentStatus::from_db_str(&r.developer_lawyer_doc_status).ok_or_else(|| {
                    unknown_enum_value(
                        "developer_lawyer_doc_status",
                        &r.developer_lawyer_doc_status,
                    )
                })?;
            let spv_lawyer_doc_status = DocumentStatus::from_db_str(&r.spv_lawyer_doc_status)
                .ok_or_else(|| {
                    unknown_enum_value("spv_lawyer_doc_status", &r.spv_lawyer_doc_status)
                })?;
            let status = ListingStatus::from_db_str(&r.status)
                .ok_or_else(|| unknown_enum_value("status", &r.status))?;
            Ok(Listing {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                listing_id: I64(r.listing_id),
                developer: b58(&r.developer),
                asset_id: I64(r.asset_id),
                share_price: I64(r.share_price),
                listed_share_amount: I64(r.listed_share_amount),
                sold_share_amount: I64(r.sold_share_amount),
                reserved_share_amount: I64(r.reserved_share_amount),
                tax_paid_by_developer: r.tax_paid_by_developer,
                tax_bps: r.tax_bps,
                marketplace_fee_bps: r.marketplace_fee_bps,
                investor_fee_bps: r.investor_fee_bps,
                max_ownership_bps: r.max_ownership_bps,
                listing_expiry: I64(r.listing_expiry),
                claiming_time: I64(r.claiming_time),
                claim_deadline: I64(r.claim_deadline),
                legal_process_time: I64(r.legal_process_time),
                lawyer_voting_time: I64(r.lawyer_voting_time),
                min_voting_quorum_bps: r.min_voting_quorum_bps,
                position_count: I64(r.position_count),
                legal_deadline: I64(r.legal_deadline),
                deposit: I64(r.deposit),
                developer_lawyer: b58(&r.developer_lawyer),
                developer_lawyer_costs: I64(r.developer_lawyer_costs),
                developer_lawyer_doc_status,
                developer_lawyer_documents_hash: hex_string(&r.developer_lawyer_documents_hash),
                spv_lawyer: b58(&r.spv_lawyer),
                spv_lawyer_costs: I64(r.spv_lawyer_costs),
                spv_lawyer_doc_status,
                spv_lawyer_documents_hash: hex_string(&r.spv_lawyer_documents_hash),
                second_attempt: r.second_attempt,
                developer_engaged: r.developer_engaged,
                spv_costs_due: I64(r.spv_costs_due),
                spv_costs_payee: b58(&r.spv_costs_payee),
                collected: json_string(&r.collected),
                spv_election_expiry: I64(r.spv_election_expiry),
                spv_election_candidate_count: I64(r.spv_election_candidate_count),
                spv_election_round: I64(r.spv_election_round),
                status,
            })
        })
        .collect::<FieldResult<Vec<_>>>()?;

    Ok(ListingConnection {
        nodes,
        total_count: total_count_i32(total),
    })
}

pub async fn property_assets(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    region_id: Option<i32>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<PropertyAssetConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, core_asset, share_mint,
               region_id, location, share_amount, spv_created, finalized, holder_count
        FROM marketplace_property_asset
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::int IS NULL OR region_id = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        region_id,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_property_asset
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::int IS NULL OR region_id = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        region_id,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(PropertyAssetConnection {
        nodes: rows
            .into_iter()
            .map(|r| PropertyAsset {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                core_asset: b58(&r.core_asset),
                share_mint: b58(&r.share_mint),
                region_id: r.region_id,
                location: utf8_lossy(&r.location),
                share_amount: I64(r.share_amount),
                spv_created: r.spv_created,
                finalized: r.finalized,
                holder_count: I64(r.holder_count),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn reservations(
    context: &GraphQLContext,
    token_account: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ReservationConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let token_account = token_account
        .as_deref()
        .map(|s| parse_b58("token_account", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, token_account, amount
        FROM marketplace_reservation
        WHERE ($1::bytea IS NULL OR token_account = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $3 OFFSET $4
        "#,
        token_account.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_reservation
        WHERE ($1::bytea IS NULL OR token_account = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        "#,
        token_account.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(ReservationConnection {
        nodes: rows
            .into_iter()
            .map(|r| Reservation {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                token_account: b58(&r.token_account),
                amount: I64(r.amount),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn share_holdings(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    owner: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ShareHoldingConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let owner = owner
        .as_deref()
        .map(|s| parse_b58("owner", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, owner, amount, locked_amount
        FROM marketplace_share_holding
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR owner = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        owner.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM marketplace_share_holding
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR owner = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        owner.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(ShareHoldingConnection {
        nodes: rows
            .into_iter()
            .map(|r| ShareHolding {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                owner: b58(&r.owner),
                amount: I64(r.amount),
                locked_amount: I64(r.locked_amount),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}
