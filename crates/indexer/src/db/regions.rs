//! Row shapes and slot-guarded writes for the `regions` program's account-state tables
//! (`migrations/0008_regions_state.sql`). Same contract as [`super::accounts`]: every upsert
//! is `ON CONFLICT (pubkey) DO UPDATE ... WHERE t.slot < EXCLUDED.slot`, closes are soft and
//! guarded, and a live write at a newer slot revives a closed row -- which this program
//! exercises routinely (RegionState/Location PDAs are closed and re-created at the same
//! address across proposal cycles).

use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

/// Mirrors the on-chain `RegionStatus` enum. The borsh variant index is load-bearing --
/// variants must stay in this exact order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionStatus {
    Proposing = 0,
    Passed = 1,
    Rejected = 2,
}

impl RegionStatus {
    pub const fn as_db_str(self) -> &'static str {
        match self {
            RegionStatus::Proposing => "PROPOSING",
            RegionStatus::Passed => "PASSED",
            RegionStatus::Rejected => "REJECTED",
        }
    }
}

/// Mirrors the on-chain `Vote` enum. Same load-bearing-index caveat as [`RegionStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vote {
    Yes = 0,
    No = 1,
    Abstain = 2,
}

impl Vote {
    pub const fn as_db_str(self) -> &'static str {
        match self {
            Vote::Yes => "YES",
            Vote::No => "NO",
            Vote::Abstain => "ABSTAIN",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RegionsConfigRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub authority: Vec<u8>,
    pub pending_authority: Option<Vec<u8>>,
    pub xcav_mint: Vec<u8>,
    pub minimum_voting_amount: i64,
    pub voting_period: i64,
    pub owner_change_period: i64,
    pub threshold_bps: i32,
    pub quorum: i64,
    pub notice_period: i64,
    pub min_vote_hold: i64,
    pub max_listing_duration: i64,
    pub max_tax_bps: i32,
    pub location_deposit: i64,
    pub proposal_counter: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct LocationRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub region_id: i32,
    /// The on-chain `bytes` postcode (1-10 bytes of uppercase ASCII), stored verbatim.
    pub postcode: Vec<u8>,
    pub deposit: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct RegionRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub region_id: i32,
    pub owner: Vec<u8>,
    pub collateral: i64,
    pub location_collateral: i64,
    pub next_owner_change: i64,
    pub listing_duration: i64,
    pub tax_bps: i32,
    pub location_count: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct RegionProposalRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub proposal_id: i64,
    pub proposer: Vec<u8>,
    pub region_id: i32,
    pub created_at: i64,
    pub expiry: i64,
    pub vote_cutoff: i64,
    pub yes_power: i64,
    pub no_power: i64,
    pub abstain_power: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct RegionStateRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub region_id: i32,
    pub status: RegionStatus,
    pub proposal_id: i64,
    pub proposer: Vec<u8>,
    pub deposit: i64,
    pub claim_deadline: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct VoteRecordRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub proposal_id: i64,
    pub voter: Vec<u8>,
    pub region_id: i32,
    pub vote: Vote,
    pub power: i64,
    pub expiry: i64,
    pub bump: i16,
}

/// One decoded regions account, ready to upsert. The account processor and the snapshot
/// loader both produce this; the batcher dispatches it to the right table.
#[derive(Debug, Clone)]
pub enum RegionsAccountRow {
    Config(RegionsConfigRow),
    Location(LocationRow),
    Region(RegionRow),
    RegionProposal(RegionProposalRow),
    RegionState(RegionStateRow),
    VoteRecord(VoteRecordRow),
}

impl RegionsAccountRow {
    pub fn slot(&self) -> i64 {
        match self {
            RegionsAccountRow::Config(r) => r.slot,
            RegionsAccountRow::Location(r) => r.slot,
            RegionsAccountRow::Region(r) => r.slot,
            RegionsAccountRow::RegionProposal(r) => r.slot,
            RegionsAccountRow::RegionState(r) => r.slot,
            RegionsAccountRow::VoteRecord(r) => r.slot,
        }
    }
}

/// Dispatch one decoded regions account to its table's slot-guarded upsert.
pub async fn upsert<'e, E>(
    executor: E,
    row: &RegionsAccountRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    match row {
        RegionsAccountRow::Config(r) => upsert_config(executor, r).await,
        RegionsAccountRow::Location(r) => upsert_location(executor, r).await,
        RegionsAccountRow::Region(r) => upsert_region(executor, r).await,
        RegionsAccountRow::RegionProposal(r) => upsert_region_proposal(executor, r).await,
        RegionsAccountRow::RegionState(r) => upsert_region_state(executor, r).await,
        RegionsAccountRow::VoteRecord(r) => upsert_vote_record(executor, r).await,
    }
}

pub async fn upsert_config<'e, E>(
    executor: E,
    row: &RegionsConfigRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO regions_config (pubkey, slot, lamports, closed_at_slot, authority,
            pending_authority, xcav_mint, minimum_voting_amount, voting_period,
            owner_change_period, threshold_bps, quorum, notice_period, min_vote_hold,
            max_listing_duration, max_tax_bps, location_deposit, proposal_counter, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot                  = EXCLUDED.slot,
            lamports              = EXCLUDED.lamports,
            closed_at_slot        = EXCLUDED.closed_at_slot,
            authority             = EXCLUDED.authority,
            pending_authority     = EXCLUDED.pending_authority,
            xcav_mint             = EXCLUDED.xcav_mint,
            minimum_voting_amount = EXCLUDED.minimum_voting_amount,
            voting_period         = EXCLUDED.voting_period,
            owner_change_period   = EXCLUDED.owner_change_period,
            threshold_bps         = EXCLUDED.threshold_bps,
            quorum                = EXCLUDED.quorum,
            notice_period         = EXCLUDED.notice_period,
            min_vote_hold         = EXCLUDED.min_vote_hold,
            max_listing_duration  = EXCLUDED.max_listing_duration,
            max_tax_bps           = EXCLUDED.max_tax_bps,
            location_deposit      = EXCLUDED.location_deposit,
            proposal_counter      = EXCLUDED.proposal_counter,
            bump                  = EXCLUDED.bump
        WHERE regions_config.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.authority,
        row.pending_authority.as_deref(),
        row.xcav_mint,
        row.minimum_voting_amount,
        row.voting_period,
        row.owner_change_period,
        row.threshold_bps,
        row.quorum,
        row.notice_period,
        row.min_vote_hold,
        row.max_listing_duration,
        row.max_tax_bps,
        row.location_deposit,
        row.proposal_counter,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_location<'e, E>(
    executor: E,
    row: &LocationRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO regions_location (pubkey, slot, lamports, closed_at_slot, region_id,
            postcode, deposit, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            region_id      = EXCLUDED.region_id,
            postcode       = EXCLUDED.postcode,
            deposit        = EXCLUDED.deposit,
            bump           = EXCLUDED.bump
        WHERE regions_location.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.region_id,
        row.postcode,
        row.deposit,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_region<'e, E>(
    executor: E,
    row: &RegionRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO regions_region (pubkey, slot, lamports, closed_at_slot, region_id, owner,
            collateral, location_collateral, next_owner_change, listing_duration, tax_bps,
            location_count, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot                = EXCLUDED.slot,
            lamports            = EXCLUDED.lamports,
            closed_at_slot      = EXCLUDED.closed_at_slot,
            region_id           = EXCLUDED.region_id,
            owner               = EXCLUDED.owner,
            collateral          = EXCLUDED.collateral,
            location_collateral = EXCLUDED.location_collateral,
            next_owner_change   = EXCLUDED.next_owner_change,
            listing_duration    = EXCLUDED.listing_duration,
            tax_bps             = EXCLUDED.tax_bps,
            location_count      = EXCLUDED.location_count,
            bump                = EXCLUDED.bump
        WHERE regions_region.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.region_id,
        row.owner,
        row.collateral,
        row.location_collateral,
        row.next_owner_change,
        row.listing_duration,
        row.tax_bps,
        row.location_count,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_region_proposal<'e, E>(
    executor: E,
    row: &RegionProposalRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO regions_region_proposal (pubkey, slot, lamports, closed_at_slot,
            proposal_id, proposer, region_id, created_at, expiry, vote_cutoff, yes_power,
            no_power, abstain_power, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            proposal_id    = EXCLUDED.proposal_id,
            proposer       = EXCLUDED.proposer,
            region_id      = EXCLUDED.region_id,
            created_at     = EXCLUDED.created_at,
            expiry         = EXCLUDED.expiry,
            vote_cutoff    = EXCLUDED.vote_cutoff,
            yes_power      = EXCLUDED.yes_power,
            no_power       = EXCLUDED.no_power,
            abstain_power  = EXCLUDED.abstain_power,
            bump           = EXCLUDED.bump
        WHERE regions_region_proposal.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.proposal_id,
        row.proposer,
        row.region_id,
        row.created_at,
        row.expiry,
        row.vote_cutoff,
        row.yes_power,
        row.no_power,
        row.abstain_power,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_region_state<'e, E>(
    executor: E,
    row: &RegionStateRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let status = row.status.as_db_str();
    sqlx::query!(
        r#"
        INSERT INTO regions_region_state (pubkey, slot, lamports, closed_at_slot, region_id,
            status, proposal_id, proposer, deposit, claim_deadline, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            region_id      = EXCLUDED.region_id,
            status         = EXCLUDED.status,
            proposal_id    = EXCLUDED.proposal_id,
            proposer       = EXCLUDED.proposer,
            deposit        = EXCLUDED.deposit,
            claim_deadline = EXCLUDED.claim_deadline,
            bump           = EXCLUDED.bump
        WHERE regions_region_state.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.region_id,
        status,
        row.proposal_id,
        row.proposer,
        row.deposit,
        row.claim_deadline,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_vote_record<'e, E>(
    executor: E,
    row: &VoteRecordRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    let vote = row.vote.as_db_str();
    sqlx::query!(
        r#"
        INSERT INTO regions_vote_record (pubkey, slot, lamports, closed_at_slot, proposal_id,
            voter, region_id, vote, power, expiry, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            proposal_id    = EXCLUDED.proposal_id,
            voter          = EXCLUDED.voter,
            region_id      = EXCLUDED.region_id,
            vote           = EXCLUDED.vote,
            power          = EXCLUDED.power,
            expiry         = EXCLUDED.expiry,
            bump           = EXCLUDED.bump
        WHERE regions_vote_record.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.proposal_id,
        row.voter,
        row.region_id,
        vote,
        row.power,
        row.expiry,
        row.bump,
    )
    .execute(executor)
    .await
}
