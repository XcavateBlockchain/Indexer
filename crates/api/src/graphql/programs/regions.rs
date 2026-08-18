//! The `regions` program's read surface: entity types over `migrations/0008_regions_state.sql`
//! and the resolver bodies `QueryRoot` delegates to. See [`super`] for the shared conventions.

use carbon_core::graphql::primitives::I64;
use juniper::{FieldResult, GraphQLObject, ID};

use super::{b58, parse_b58, total_count_i32, utf8_lossy};
use crate::graphql::context::GraphQLContext;
use crate::graphql::enums::{unknown_enum_value, RegionStatus, RegionVote};
use crate::guards::{clamp_first, clamp_offset};

/// The regions program's Config PDA (singleton). `null` until `initialize_config` has been
/// indexed.
#[derive(GraphQLObject, Clone, Debug)]
pub struct RegionsConfig {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub authority: String,
    pub pending_authority: Option<String>,
    pub xcav_mint: String,
    pub minimum_voting_amount: I64,
    pub voting_period: I64,
    pub owner_change_period: I64,
    pub threshold_bps: i32,
    pub quorum: I64,
    pub notice_period: I64,
    pub min_vote_hold: I64,
    pub max_listing_duration: I64,
    pub max_tax_bps: i32,
    pub location_deposit: I64,
    pub proposal_counter: I64,
}

/// A Location PDA: one registered postcode within a region. `postcode` is the on-chain byte
/// string (1-10 bytes of uppercase ASCII) rendered as UTF-8.
#[derive(GraphQLObject, Clone, Debug)]
pub struct Location {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub region_id: i32,
    pub postcode: String,
    pub deposit: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct LocationConnection {
    pub nodes: Vec<Location>,
    pub total_count: i32,
}

/// A Region PDA: one created region with its owner, collateral and listing parameters.
#[derive(GraphQLObject, Clone, Debug)]
pub struct Region {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub region_id: i32,
    pub owner: String,
    pub collateral: I64,
    pub location_collateral: I64,
    pub next_owner_change: I64,
    pub listing_duration: I64,
    pub tax_bps: i32,
    pub location_count: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct RegionConnection {
    pub nodes: Vec<Region>,
    pub total_count: i32,
}

/// A RegionProposal PDA: one proposal to create a region, with its vote tallies. `proposal_id`
/// is the global monotonic `config.proposal_counter`, not the region id.
#[derive(GraphQLObject, Clone, Debug)]
pub struct RegionProposal {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub proposal_id: I64,
    pub proposer: String,
    pub region_id: i32,
    pub created_at: I64,
    pub expiry: I64,
    pub vote_cutoff: I64,
    pub yes_power: I64,
    pub no_power: I64,
    pub abstain_power: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct RegionProposalConnection {
    pub nodes: Vec<RegionProposal>,
    pub total_count: i32,
}

/// A RegionState PDA: the per-region proposal-cycle state machine, closed and re-created at
/// the same address across proposal cycles.
#[derive(GraphQLObject, Clone, Debug)]
pub struct RegionState {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub region_id: i32,
    pub status: RegionStatus,
    pub proposal_id: I64,
    pub proposer: String,
    pub deposit: I64,
    pub claim_deadline: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct RegionStateConnection {
    pub nodes: Vec<RegionState>,
    pub total_count: i32,
}

/// A VoteRecord PDA: one voter's vote on one region proposal.
#[derive(GraphQLObject, Clone, Debug)]
pub struct VoteRecord {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub proposal_id: I64,
    pub voter: String,
    pub region_id: i32,
    pub vote: RegionVote,
    pub power: I64,
    pub expiry: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct VoteRecordConnection {
    pub nodes: Vec<VoteRecord>,
    pub total_count: i32,
}

// --- resolver bodies ------------------------------------------------------------------------

pub async fn regions_config(context: &GraphQLContext) -> FieldResult<Option<RegionsConfig>> {
    let row = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, authority, pending_authority, xcav_mint,
               minimum_voting_amount, voting_period, owner_change_period, threshold_bps, quorum,
               notice_period, min_vote_hold, max_listing_duration, max_tax_bps, location_deposit,
               proposal_counter
        FROM regions_config
        ORDER BY slot DESC
        LIMIT 1
        "#
    )
    .fetch_optional(&context.pool)
    .await?;
    Ok(row.map(|r| RegionsConfig {
        id: ID::new(b58(&r.pubkey)),
        slot: I64(r.slot),
        lamports: I64(r.lamports),
        active: r.closed_at_slot.is_none(),
        closed_at_slot: r.closed_at_slot.map(I64),
        authority: b58(&r.authority),
        pending_authority: r.pending_authority.as_deref().map(b58),
        xcav_mint: b58(&r.xcav_mint),
        minimum_voting_amount: I64(r.minimum_voting_amount),
        voting_period: I64(r.voting_period),
        owner_change_period: I64(r.owner_change_period),
        threshold_bps: r.threshold_bps,
        quorum: I64(r.quorum),
        notice_period: I64(r.notice_period),
        min_vote_hold: I64(r.min_vote_hold),
        max_listing_duration: I64(r.max_listing_duration),
        max_tax_bps: r.max_tax_bps,
        location_deposit: I64(r.location_deposit),
        proposal_counter: I64(r.proposal_counter),
    }))
}

pub async fn locations(
    context: &GraphQLContext,
    region_id: Option<i32>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<LocationConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, region_id, postcode, deposit
        FROM regions_location
        WHERE ($1::int IS NULL OR region_id = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $3 OFFSET $4
        "#,
        region_id,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM regions_location
        WHERE ($1::int IS NULL OR region_id = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        "#,
        region_id,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(LocationConnection {
        nodes: rows
            .into_iter()
            .map(|r| Location {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                region_id: r.region_id,
                postcode: utf8_lossy(&r.postcode),
                deposit: I64(r.deposit),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn regions(
    context: &GraphQLContext,
    region_id: Option<i32>,
    owner: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<RegionConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let owner = owner
        .as_deref()
        .map(|s| parse_b58("owner", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, region_id, owner, collateral,
               location_collateral, next_owner_change, listing_duration, tax_bps, location_count
        FROM regions_region
        WHERE ($1::int IS NULL OR region_id = $1)
          AND ($2::bytea IS NULL OR owner = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        region_id,
        owner.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM regions_region
        WHERE ($1::int IS NULL OR region_id = $1)
          AND ($2::bytea IS NULL OR owner = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        region_id,
        owner.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(RegionConnection {
        nodes: rows
            .into_iter()
            .map(|r| Region {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                region_id: r.region_id,
                owner: b58(&r.owner),
                collateral: I64(r.collateral),
                location_collateral: I64(r.location_collateral),
                next_owner_change: I64(r.next_owner_change),
                listing_duration: I64(r.listing_duration),
                tax_bps: r.tax_bps,
                location_count: I64(r.location_count),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn region_proposals(
    context: &GraphQLContext,
    region_id: Option<i32>,
    proposal_id: Option<I64>,
    proposer: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<RegionProposalConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let proposal_id = proposal_id.map(|v| v.0);
    let proposer = proposer
        .as_deref()
        .map(|s| parse_b58("proposer", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, proposal_id, proposer, region_id,
               created_at, expiry, vote_cutoff, yes_power, no_power, abstain_power
        FROM regions_region_proposal
        WHERE ($1::int IS NULL OR region_id = $1)
          AND ($2::bigint IS NULL OR proposal_id = $2)
          AND ($3::bytea IS NULL OR proposer = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $5 OFFSET $6
        "#,
        region_id,
        proposal_id,
        proposer.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM regions_region_proposal
        WHERE ($1::int IS NULL OR region_id = $1)
          AND ($2::bigint IS NULL OR proposal_id = $2)
          AND ($3::bytea IS NULL OR proposer = $3)
          AND ($4::bool IS NULL OR (closed_at_slot IS NULL) = $4)
        "#,
        region_id,
        proposal_id,
        proposer.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(RegionProposalConnection {
        nodes: rows
            .into_iter()
            .map(|r| RegionProposal {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                proposal_id: I64(r.proposal_id),
                proposer: b58(&r.proposer),
                region_id: r.region_id,
                created_at: I64(r.created_at),
                expiry: I64(r.expiry),
                vote_cutoff: I64(r.vote_cutoff),
                yes_power: I64(r.yes_power),
                no_power: I64(r.no_power),
                abstain_power: I64(r.abstain_power),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn region_states(
    context: &GraphQLContext,
    region_id: Option<i32>,
    status: Option<RegionStatus>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<RegionStateConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let status = status.map(|s| s.as_db_str());

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, region_id, status, proposal_id, proposer,
               deposit, claim_deadline
        FROM regions_region_state
        WHERE ($1::int IS NULL OR region_id = $1)
          AND ($2::text IS NULL OR status = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        region_id,
        status,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM regions_region_state
        WHERE ($1::int IS NULL OR region_id = $1)
          AND ($2::text IS NULL OR status = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        region_id,
        status,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    let nodes = rows
        .into_iter()
        .map(|r| {
            let status = RegionStatus::from_db_str(&r.status)
                .ok_or_else(|| unknown_enum_value("status", &r.status))?;
            Ok(RegionState {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                region_id: r.region_id,
                status,
                proposal_id: I64(r.proposal_id),
                proposer: b58(&r.proposer),
                deposit: I64(r.deposit),
                claim_deadline: I64(r.claim_deadline),
            })
        })
        .collect::<FieldResult<Vec<_>>>()?;

    Ok(RegionStateConnection {
        nodes,
        total_count: total_count_i32(total),
    })
}

pub async fn vote_records(
    context: &GraphQLContext,
    proposal_id: Option<I64>,
    voter: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<VoteRecordConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let proposal_id = proposal_id.map(|v| v.0);
    let voter = voter
        .as_deref()
        .map(|s| parse_b58("voter", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, proposal_id, voter, region_id, vote,
               power, expiry
        FROM regions_vote_record
        WHERE ($1::bigint IS NULL OR proposal_id = $1)
          AND ($2::bytea IS NULL OR voter = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        proposal_id,
        voter.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM regions_vote_record
        WHERE ($1::bigint IS NULL OR proposal_id = $1)
          AND ($2::bytea IS NULL OR voter = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        proposal_id,
        voter.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    let nodes = rows
        .into_iter()
        .map(|r| {
            let vote = RegionVote::from_db_str(&r.vote)
                .ok_or_else(|| unknown_enum_value("vote", &r.vote))?;
            Ok(VoteRecord {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                proposal_id: I64(r.proposal_id),
                voter: b58(&r.voter),
                region_id: r.region_id,
                vote,
                power: I64(r.power),
                expiry: I64(r.expiry),
            })
        })
        .collect::<FieldResult<Vec<_>>>()?;

    Ok(VoteRecordConnection {
        nodes,
        total_count: total_count_i32(total),
    })
}
