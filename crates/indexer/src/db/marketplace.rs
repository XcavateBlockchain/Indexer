//! Row shapes and slot-guarded writes for the `marketplace` program's account-state tables
//! (`migrations/0009_marketplace_state.sql`). Same contract as [`super::accounts`].
//!
//! Shape notes (see the migration header): the `LawyerAssignment` and `SpvElection` nested
//! structs are flattened into typed columns; `accepted_payment_mints` and `collected` are
//! JSONB lists in shapes this module's callers construct (pubkeys as base58 strings);
//! `PropertyAsset.location` is the raw postcode byte string.

use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

/// Mirrors the on-chain `ListingStatus` enum. The borsh variant index is load-bearing --
/// variants must stay in this exact order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingStatus {
    PendingAssets = 0,
    Listed = 1,
    SoldOut = 2,
    Legal = 3,
    Finalized = 4,
    Expired = 5,
    Cancelled = 6,
    Refunding = 7,
}

impl ListingStatus {
    pub const fn as_db_str(self) -> &'static str {
        match self {
            ListingStatus::PendingAssets => "PENDING_ASSETS",
            ListingStatus::Listed => "LISTED",
            ListingStatus::SoldOut => "SOLD_OUT",
            ListingStatus::Legal => "LEGAL",
            ListingStatus::Finalized => "FINALIZED",
            ListingStatus::Expired => "EXPIRED",
            ListingStatus::Cancelled => "CANCELLED",
            ListingStatus::Refunding => "REFUNDING",
        }
    }
}

/// Mirrors the on-chain `DocumentStatus` enum (inside `LawyerAssignment`). Same
/// load-bearing-index caveat as [`ListingStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentStatus {
    Pending = 0,
    Approved = 1,
    Rejected = 2,
}

impl DocumentStatus {
    pub const fn as_db_str(self) -> &'static str {
        match self {
            DocumentStatus::Pending => "PENDING",
            DocumentStatus::Approved => "APPROVED",
            DocumentStatus::Rejected => "REJECTED",
        }
    }
}

/// The on-chain `LawyerAssignment` nested struct, flattened into the four `*_lawyer*`
/// column groups of `marketplace_listing`.
#[derive(Debug, Clone)]
pub struct LawyerAssignmentCols {
    pub lawyer: Vec<u8>,
    pub costs: i64,
    pub doc_status: DocumentStatus,
    pub documents_hash: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct MarketplaceConfigRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub authority: Vec<u8>,
    pub pending_authority: Option<Vec<u8>>,
    pub xcav_mint: Vec<u8>,
    pub treasury: Vec<u8>,
    pub rent_collector: Vec<u8>,
    /// JSONB array of base58 strings.
    pub accepted_payment_mints: serde_json::Value,
    pub listing_deposit: i64,
    pub lawyer_deposit: i64,
    pub min_property_shares: i64,
    pub max_property_shares: i64,
    pub marketplace_fee_bps: i32,
    pub investor_fee_bps: i32,
    pub max_ownership_bps: i32,
    pub claiming_time: i64,
    pub legal_process_time: i64,
    pub lawyer_voting_time: i64,
    pub min_voting_quorum_bps: i32,
    pub next_listing_id: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct InvestorPositionRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub listing_id: i64,
    pub investor: Vec<u8>,
    pub payment_mint: Vec<u8>,
    pub payment_account: Vec<u8>,
    pub share_amount: i64,
    pub reserved_share_amount: i64,
    pub paid_funds: i64,
    pub paid_tax: i64,
    pub paid_fee: i64,
    pub reserved_funds: i64,
    pub reserved_tax: i64,
    pub reserved_fee: i64,
    pub cancelled: bool,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct LawyerRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub lawyer: Vec<u8>,
    pub region_id: i32,
    pub deposit: i64,
    pub active_cases: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct LawyerCandidacyRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub listing_id: i64,
    pub round: i64,
    pub lawyer: Vec<u8>,
    pub costs: i64,
    pub vote_power: i64,
    pub rent_payer: Vec<u8>,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct LawyerVoteRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub listing_id: i64,
    pub round: i64,
    pub voter: Vec<u8>,
    pub choice: Vec<u8>,
    pub power: i64,
    pub rent_payer: Vec<u8>,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct ListingRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub listing_id: i64,
    pub developer: Vec<u8>,
    pub asset_id: i64,
    pub share_price: i64,
    pub listed_share_amount: i64,
    pub sold_share_amount: i64,
    pub reserved_share_amount: i64,
    pub tax_paid_by_developer: bool,
    pub tax_bps: i32,
    pub marketplace_fee_bps: i32,
    pub investor_fee_bps: i32,
    pub max_ownership_bps: i32,
    pub listing_expiry: i64,
    pub claiming_time: i64,
    pub claim_deadline: i64,
    pub legal_process_time: i64,
    pub lawyer_voting_time: i64,
    pub min_voting_quorum_bps: i32,
    pub position_count: i64,
    pub legal_deadline: i64,
    pub deposit: i64,
    pub developer_lawyer: LawyerAssignmentCols,
    pub spv_lawyer: LawyerAssignmentCols,
    pub second_attempt: bool,
    pub developer_engaged: bool,
    pub spv_costs_due: i64,
    pub spv_costs_payee: Vec<u8>,
    /// JSONB array of `{"mint": "<base58>", "funds": N, "fee": N, "tax": N}`.
    pub collected: serde_json::Value,
    pub spv_election_expiry: i64,
    pub spv_election_candidate_count: i64,
    pub spv_election_round: i64,
    pub status: ListingStatus,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct PropertyAssetRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub asset_id: i64,
    pub core_asset: Vec<u8>,
    pub share_mint: Vec<u8>,
    pub region_id: i32,
    /// Raw postcode byte string.
    pub location: Vec<u8>,
    pub share_amount: i64,
    pub spv_created: bool,
    pub finalized: bool,
    pub holder_count: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct ReservationRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub token_account: Vec<u8>,
    pub amount: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct ShareHoldingRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub asset_id: i64,
    pub owner: Vec<u8>,
    pub amount: i64,
    pub locked_amount: i64,
    pub bump: i16,
}

/// One decoded marketplace account, ready to upsert.
#[derive(Debug, Clone)]
pub enum MarketplaceAccountRow {
    Config(MarketplaceConfigRow),
    InvestorPosition(InvestorPositionRow),
    Lawyer(LawyerRow),
    LawyerCandidacy(LawyerCandidacyRow),
    LawyerVote(LawyerVoteRow),
    Listing(Box<ListingRow>),
    PropertyAsset(PropertyAssetRow),
    Reservation(ReservationRow),
    ShareHolding(ShareHoldingRow),
}

impl MarketplaceAccountRow {
    pub fn slot(&self) -> i64 {
        match self {
            MarketplaceAccountRow::Config(r) => r.slot,
            MarketplaceAccountRow::InvestorPosition(r) => r.slot,
            MarketplaceAccountRow::Lawyer(r) => r.slot,
            MarketplaceAccountRow::LawyerCandidacy(r) => r.slot,
            MarketplaceAccountRow::LawyerVote(r) => r.slot,
            MarketplaceAccountRow::Listing(r) => r.slot,
            MarketplaceAccountRow::PropertyAsset(r) => r.slot,
            MarketplaceAccountRow::Reservation(r) => r.slot,
            MarketplaceAccountRow::ShareHolding(r) => r.slot,
        }
    }
}

/// Dispatch one decoded marketplace account to its table's slot-guarded upsert.
pub async fn upsert<'e, E>(
    executor: E,
    row: &MarketplaceAccountRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    match row {
        MarketplaceAccountRow::Config(r) => upsert_config(executor, r).await,
        MarketplaceAccountRow::InvestorPosition(r) => upsert_investor_position(executor, r).await,
        MarketplaceAccountRow::Lawyer(r) => upsert_lawyer(executor, r).await,
        MarketplaceAccountRow::LawyerCandidacy(r) => upsert_lawyer_candidacy(executor, r).await,
        MarketplaceAccountRow::LawyerVote(r) => upsert_lawyer_vote(executor, r).await,
        MarketplaceAccountRow::Listing(r) => upsert_listing(executor, r).await,
        MarketplaceAccountRow::PropertyAsset(r) => upsert_property_asset(executor, r).await,
        MarketplaceAccountRow::Reservation(r) => upsert_reservation(executor, r).await,
        MarketplaceAccountRow::ShareHolding(r) => upsert_share_holding(executor, r).await,
    }
}

pub async fn upsert_config<'e, E>(
    executor: E,
    row: &MarketplaceConfigRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_config (pubkey, slot, lamports, closed_at_slot, authority,
            pending_authority, xcav_mint, treasury, rent_collector, accepted_payment_mints,
            listing_deposit, lawyer_deposit, min_property_shares, max_property_shares,
            marketplace_fee_bps, investor_fee_bps, max_ownership_bps, claiming_time,
            legal_process_time, lawyer_voting_time, min_voting_quorum_bps, next_listing_id, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                $17, $18, $19, $20, $21, $22)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot                   = EXCLUDED.slot,
            lamports               = EXCLUDED.lamports,
            closed_at_slot         = EXCLUDED.closed_at_slot,
            authority              = EXCLUDED.authority,
            pending_authority      = EXCLUDED.pending_authority,
            xcav_mint              = EXCLUDED.xcav_mint,
            treasury               = EXCLUDED.treasury,
            rent_collector         = EXCLUDED.rent_collector,
            accepted_payment_mints = EXCLUDED.accepted_payment_mints,
            listing_deposit        = EXCLUDED.listing_deposit,
            lawyer_deposit         = EXCLUDED.lawyer_deposit,
            min_property_shares    = EXCLUDED.min_property_shares,
            max_property_shares    = EXCLUDED.max_property_shares,
            marketplace_fee_bps    = EXCLUDED.marketplace_fee_bps,
            investor_fee_bps       = EXCLUDED.investor_fee_bps,
            max_ownership_bps      = EXCLUDED.max_ownership_bps,
            claiming_time          = EXCLUDED.claiming_time,
            legal_process_time     = EXCLUDED.legal_process_time,
            lawyer_voting_time     = EXCLUDED.lawyer_voting_time,
            min_voting_quorum_bps  = EXCLUDED.min_voting_quorum_bps,
            next_listing_id        = EXCLUDED.next_listing_id,
            bump                   = EXCLUDED.bump
        WHERE marketplace_config.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.authority,
        row.pending_authority.as_deref(),
        row.xcav_mint,
        row.treasury,
        row.rent_collector,
        row.accepted_payment_mints,
        row.listing_deposit,
        row.lawyer_deposit,
        row.min_property_shares,
        row.max_property_shares,
        row.marketplace_fee_bps,
        row.investor_fee_bps,
        row.max_ownership_bps,
        row.claiming_time,
        row.legal_process_time,
        row.lawyer_voting_time,
        row.min_voting_quorum_bps,
        row.next_listing_id,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_investor_position<'e, E>(
    executor: E,
    row: &InvestorPositionRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_investor_position (pubkey, slot, lamports, closed_at_slot,
            listing_id, investor, payment_mint, payment_account, share_amount,
            reserved_share_amount, paid_funds, paid_tax, paid_fee, reserved_funds,
            reserved_tax, reserved_fee, cancelled, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot                  = EXCLUDED.slot,
            lamports              = EXCLUDED.lamports,
            closed_at_slot        = EXCLUDED.closed_at_slot,
            listing_id            = EXCLUDED.listing_id,
            investor              = EXCLUDED.investor,
            payment_mint          = EXCLUDED.payment_mint,
            payment_account       = EXCLUDED.payment_account,
            share_amount          = EXCLUDED.share_amount,
            reserved_share_amount = EXCLUDED.reserved_share_amount,
            paid_funds            = EXCLUDED.paid_funds,
            paid_tax              = EXCLUDED.paid_tax,
            paid_fee              = EXCLUDED.paid_fee,
            reserved_funds        = EXCLUDED.reserved_funds,
            reserved_tax          = EXCLUDED.reserved_tax,
            reserved_fee          = EXCLUDED.reserved_fee,
            cancelled             = EXCLUDED.cancelled,
            bump                  = EXCLUDED.bump
        WHERE marketplace_investor_position.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.listing_id,
        row.investor,
        row.payment_mint,
        row.payment_account,
        row.share_amount,
        row.reserved_share_amount,
        row.paid_funds,
        row.paid_tax,
        row.paid_fee,
        row.reserved_funds,
        row.reserved_tax,
        row.reserved_fee,
        row.cancelled,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_lawyer<'e, E>(
    executor: E,
    row: &LawyerRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_lawyer (pubkey, slot, lamports, closed_at_slot, lawyer,
            region_id, deposit, active_cases, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            lawyer         = EXCLUDED.lawyer,
            region_id      = EXCLUDED.region_id,
            deposit        = EXCLUDED.deposit,
            active_cases   = EXCLUDED.active_cases,
            bump           = EXCLUDED.bump
        WHERE marketplace_lawyer.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.lawyer,
        row.region_id,
        row.deposit,
        row.active_cases,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_lawyer_candidacy<'e, E>(
    executor: E,
    row: &LawyerCandidacyRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_lawyer_candidacy (pubkey, slot, lamports, closed_at_slot,
            listing_id, round, lawyer, costs, vote_power, rent_payer, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            listing_id     = EXCLUDED.listing_id,
            round          = EXCLUDED.round,
            lawyer         = EXCLUDED.lawyer,
            costs          = EXCLUDED.costs,
            vote_power     = EXCLUDED.vote_power,
            rent_payer     = EXCLUDED.rent_payer,
            bump           = EXCLUDED.bump
        WHERE marketplace_lawyer_candidacy.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.listing_id,
        row.round,
        row.lawyer,
        row.costs,
        row.vote_power,
        row.rent_payer,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_lawyer_vote<'e, E>(
    executor: E,
    row: &LawyerVoteRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_lawyer_vote (pubkey, slot, lamports, closed_at_slot,
            listing_id, round, voter, choice, power, rent_payer, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            listing_id     = EXCLUDED.listing_id,
            round          = EXCLUDED.round,
            voter          = EXCLUDED.voter,
            choice         = EXCLUDED.choice,
            power          = EXCLUDED.power,
            rent_payer     = EXCLUDED.rent_payer,
            bump           = EXCLUDED.bump
        WHERE marketplace_lawyer_vote.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.listing_id,
        row.round,
        row.voter,
        row.choice,
        row.power,
        row.rent_payer,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_listing<'e, E>(
    executor: E,
    row: &ListingRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let developer_doc_status = row.developer_lawyer.doc_status.as_db_str();
    let spv_doc_status = row.spv_lawyer.doc_status.as_db_str();
    let status = row.status.as_db_str();
    sqlx::query!(
        r#"
        INSERT INTO marketplace_listing (pubkey, slot, lamports, closed_at_slot, listing_id,
            developer, asset_id, share_price, listed_share_amount, sold_share_amount,
            reserved_share_amount, tax_paid_by_developer, tax_bps, marketplace_fee_bps,
            investor_fee_bps, max_ownership_bps, listing_expiry, claiming_time, claim_deadline,
            legal_process_time, lawyer_voting_time, min_voting_quorum_bps, position_count,
            legal_deadline, deposit,
            developer_lawyer, developer_lawyer_costs, developer_lawyer_doc_status,
            developer_lawyer_documents_hash,
            spv_lawyer, spv_lawyer_costs, spv_lawyer_doc_status, spv_lawyer_documents_hash,
            second_attempt, developer_engaged, spv_costs_due, spv_costs_payee, collected,
            spv_election_expiry, spv_election_candidate_count, spv_election_round, status, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29, $30, $31,
                $32, $33, $34, $35, $36, $37, $38, $39, $40, $41, $42)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot                            = EXCLUDED.slot,
            lamports                        = EXCLUDED.lamports,
            closed_at_slot                  = EXCLUDED.closed_at_slot,
            listing_id                      = EXCLUDED.listing_id,
            developer                       = EXCLUDED.developer,
            asset_id                        = EXCLUDED.asset_id,
            share_price                     = EXCLUDED.share_price,
            listed_share_amount             = EXCLUDED.listed_share_amount,
            sold_share_amount               = EXCLUDED.sold_share_amount,
            reserved_share_amount           = EXCLUDED.reserved_share_amount,
            tax_paid_by_developer           = EXCLUDED.tax_paid_by_developer,
            tax_bps                         = EXCLUDED.tax_bps,
            marketplace_fee_bps             = EXCLUDED.marketplace_fee_bps,
            investor_fee_bps                = EXCLUDED.investor_fee_bps,
            max_ownership_bps               = EXCLUDED.max_ownership_bps,
            listing_expiry                  = EXCLUDED.listing_expiry,
            claiming_time                   = EXCLUDED.claiming_time,
            claim_deadline                  = EXCLUDED.claim_deadline,
            legal_process_time              = EXCLUDED.legal_process_time,
            lawyer_voting_time              = EXCLUDED.lawyer_voting_time,
            min_voting_quorum_bps           = EXCLUDED.min_voting_quorum_bps,
            position_count                  = EXCLUDED.position_count,
            legal_deadline                  = EXCLUDED.legal_deadline,
            deposit                         = EXCLUDED.deposit,
            developer_lawyer                = EXCLUDED.developer_lawyer,
            developer_lawyer_costs          = EXCLUDED.developer_lawyer_costs,
            developer_lawyer_doc_status     = EXCLUDED.developer_lawyer_doc_status,
            developer_lawyer_documents_hash = EXCLUDED.developer_lawyer_documents_hash,
            spv_lawyer                      = EXCLUDED.spv_lawyer,
            spv_lawyer_costs                = EXCLUDED.spv_lawyer_costs,
            spv_lawyer_doc_status           = EXCLUDED.spv_lawyer_doc_status,
            spv_lawyer_documents_hash       = EXCLUDED.spv_lawyer_documents_hash,
            second_attempt                  = EXCLUDED.second_attempt,
            developer_engaged               = EXCLUDED.developer_engaged,
            spv_costs_due                   = EXCLUDED.spv_costs_due,
            spv_costs_payee                 = EXCLUDED.spv_costs_payee,
            collected                       = EXCLUDED.collected,
            spv_election_expiry             = EXCLUDED.spv_election_expiry,
            spv_election_candidate_count    = EXCLUDED.spv_election_candidate_count,
            spv_election_round              = EXCLUDED.spv_election_round,
            status                          = EXCLUDED.status,
            bump                            = EXCLUDED.bump
        WHERE marketplace_listing.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.listing_id,
        row.developer,
        row.asset_id,
        row.share_price,
        row.listed_share_amount,
        row.sold_share_amount,
        row.reserved_share_amount,
        row.tax_paid_by_developer,
        row.tax_bps,
        row.marketplace_fee_bps,
        row.investor_fee_bps,
        row.max_ownership_bps,
        row.listing_expiry,
        row.claiming_time,
        row.claim_deadline,
        row.legal_process_time,
        row.lawyer_voting_time,
        row.min_voting_quorum_bps,
        row.position_count,
        row.legal_deadline,
        row.deposit,
        row.developer_lawyer.lawyer,
        row.developer_lawyer.costs,
        developer_doc_status,
        row.developer_lawyer.documents_hash,
        row.spv_lawyer.lawyer,
        row.spv_lawyer.costs,
        spv_doc_status,
        row.spv_lawyer.documents_hash,
        row.second_attempt,
        row.developer_engaged,
        row.spv_costs_due,
        row.spv_costs_payee,
        row.collected,
        row.spv_election_expiry,
        row.spv_election_candidate_count,
        row.spv_election_round,
        status,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_property_asset<'e, E>(
    executor: E,
    row: &PropertyAssetRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_property_asset (pubkey, slot, lamports, closed_at_slot,
            asset_id, core_asset, share_mint, region_id, location, share_amount, spv_created,
            finalized, holder_count, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            asset_id       = EXCLUDED.asset_id,
            core_asset     = EXCLUDED.core_asset,
            share_mint     = EXCLUDED.share_mint,
            region_id      = EXCLUDED.region_id,
            location       = EXCLUDED.location,
            share_amount   = EXCLUDED.share_amount,
            spv_created    = EXCLUDED.spv_created,
            finalized      = EXCLUDED.finalized,
            holder_count   = EXCLUDED.holder_count,
            bump           = EXCLUDED.bump
        WHERE marketplace_property_asset.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.asset_id,
        row.core_asset,
        row.share_mint,
        row.region_id,
        row.location,
        row.share_amount,
        row.spv_created,
        row.finalized,
        row.holder_count,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_reservation<'e, E>(
    executor: E,
    row: &ReservationRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_reservation (pubkey, slot, lamports, closed_at_slot,
            token_account, amount, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            token_account  = EXCLUDED.token_account,
            amount         = EXCLUDED.amount,
            bump           = EXCLUDED.bump
        WHERE marketplace_reservation.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.token_account,
        row.amount,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_share_holding<'e, E>(
    executor: E,
    row: &ShareHoldingRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO marketplace_share_holding (pubkey, slot, lamports, closed_at_slot,
            asset_id, owner, amount, locked_amount, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            asset_id       = EXCLUDED.asset_id,
            owner          = EXCLUDED.owner,
            amount         = EXCLUDED.amount,
            locked_amount  = EXCLUDED.locked_amount,
            bump           = EXCLUDED.bump
        WHERE marketplace_share_holding.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.asset_id,
        row.owner,
        row.amount,
        row.locked_amount,
        row.bump,
    )
    .execute(executor)
    .await
}
