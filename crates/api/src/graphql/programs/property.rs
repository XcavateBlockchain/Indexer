//! The `property` program's read surface: entity types over `migrations/0010_property_state.sql`
//! and the resolver bodies `QueryRoot` delegates to. See [`super`] for the shared conventions.

use carbon_core::graphql::primitives::I64;
use juniper::{FieldResult, GraphQLObject, ID};

use super::{b58, json_string, parse_b58, total_count_i32};
use crate::graphql::context::GraphQLContext;
use crate::guards::{clamp_first, clamp_offset};

/// The property program's Config PDA (singleton). `null` until `initialize_config` has been
/// indexed.
#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyConfig {
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
    pub agent_deposit: I64,
    pub agent_voting_time: I64,
    pub min_voting_quorum_bps: i32,
    pub agent_notice_period: I64,
}

/// A letting-agent candidacy for one property's election round.
#[derive(GraphQLObject, Clone, Debug)]
pub struct AgentCandidacy {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub round: I64,
    pub agent: String,
    pub vote_power: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct AgentCandidacyConnection {
    pub nodes: Vec<AgentCandidacy>,
    pub total_count: i32,
}

/// One voter's vote in a letting-agent election round.
#[derive(GraphQLObject, Clone, Debug)]
pub struct AgentVote {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub round: I64,
    pub voter: String,
    pub choice: String,
    pub power: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct AgentVoteConnection {
    pub nodes: Vec<AgentVote>,
    pub total_count: i32,
}

/// A registered letting agent. `locations` is the raw JSON of the on-chain location list
/// (`[{"postcode": "...", "assigned_count": N, "deposit": N}, ...]` -- see migration 0010).
#[derive(GraphQLObject, Clone, Debug)]
pub struct LettingAgent {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub wallet: String,
    pub region_id: i32,
    pub locations: String,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct LettingAgentConnection {
    pub nodes: Vec<LettingAgent>,
    pub total_count: i32,
}

/// One property's letting seat and current election. `agent` keeps the on-chain all-zero
/// pubkey "seat vacant" sentinel verbatim (base58 `11111111111111111111111111111111`).
#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyLetting {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub agent: String,
    pub election_expiry: I64,
    pub election_candidate_count: I64,
    pub election_round: I64,
    pub election_quorum_bps: i32,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyLettingConnection {
    pub nodes: Vec<PropertyLetting>,
    pub total_count: i32,
}

/// A letting agent's notice of resignation from one property.
#[derive(GraphQLObject, Clone, Debug)]
pub struct ResignationNotice {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub agent: String,
    pub due_ts: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ResignationNoticeConnection {
    pub nodes: Vec<ResignationNotice>,
    pub total_count: i32,
}

// --- resolver bodies ------------------------------------------------------------------------

pub async fn property_config(context: &GraphQLContext) -> FieldResult<Option<PropertyConfig>> {
    let row = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, authority, pending_authority, xcav_mint,
               treasury, rent_collector, agent_deposit, agent_voting_time,
               min_voting_quorum_bps, agent_notice_period
        FROM property_config
        ORDER BY slot DESC
        LIMIT 1
        "#
    )
    .fetch_optional(&context.pool)
    .await?;
    Ok(row.map(|r| PropertyConfig {
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
        agent_deposit: I64(r.agent_deposit),
        agent_voting_time: I64(r.agent_voting_time),
        min_voting_quorum_bps: r.min_voting_quorum_bps,
        agent_notice_period: I64(r.agent_notice_period),
    }))
}

pub async fn agent_candidacies(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    agent: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<AgentCandidacyConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let agent = agent
        .as_deref()
        .map(|s| parse_b58("agent", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, round, agent, vote_power,
               rent_payer
        FROM property_agent_candidacy
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR agent = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        agent.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM property_agent_candidacy
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR agent = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        agent.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(AgentCandidacyConnection {
        nodes: rows
            .into_iter()
            .map(|r| AgentCandidacy {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                round: I64(r.round),
                agent: b58(&r.agent),
                vote_power: I64(r.vote_power),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn agent_votes(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    voter: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<AgentVoteConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let voter = voter
        .as_deref()
        .map(|s| parse_b58("voter", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, round, voter, choice, power,
               rent_payer
        FROM property_agent_vote
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR voter = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        voter.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM property_agent_vote
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR voter = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        voter.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(AgentVoteConnection {
        nodes: rows
            .into_iter()
            .map(|r| AgentVote {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
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

pub async fn letting_agents(
    context: &GraphQLContext,
    wallet: Option<String>,
    region_id: Option<i32>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<LettingAgentConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let wallet = wallet
        .as_deref()
        .map(|s| parse_b58("wallet", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, wallet, region_id, locations, rent_payer
        FROM property_letting_agent
        WHERE ($1::bytea IS NULL OR wallet = $1)
          AND ($2::int IS NULL OR region_id = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        wallet.as_deref(),
        region_id,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM property_letting_agent
        WHERE ($1::bytea IS NULL OR wallet = $1)
          AND ($2::int IS NULL OR region_id = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        wallet.as_deref(),
        region_id,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(LettingAgentConnection {
        nodes: rows
            .into_iter()
            .map(|r| LettingAgent {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                wallet: b58(&r.wallet),
                region_id: r.region_id,
                locations: json_string(&r.locations),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn property_lettings(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    agent: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<PropertyLettingConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let agent = agent
        .as_deref()
        .map(|s| parse_b58("agent", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, agent, election_expiry,
               election_candidate_count, election_round, election_quorum_bps, rent_payer
        FROM property_letting
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR agent = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        agent.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM property_letting
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR agent = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        agent.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(PropertyLettingConnection {
        nodes: rows
            .into_iter()
            .map(|r| PropertyLetting {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                agent: b58(&r.agent),
                election_expiry: I64(r.election_expiry),
                election_candidate_count: I64(r.election_candidate_count),
                election_round: I64(r.election_round),
                election_quorum_bps: r.election_quorum_bps,
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn resignation_notices(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    agent: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ResignationNoticeConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let agent = agent
        .as_deref()
        .map(|s| parse_b58("agent", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, agent, due_ts, rent_payer
        FROM property_resignation_notice
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR agent = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        agent.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM property_resignation_notice
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR agent = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        agent.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(ResignationNoticeConnection {
        nodes: rows
            .into_iter()
            .map(|r| ResignationNotice {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                agent: b58(&r.agent),
                due_ts: I64(r.due_ts),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}
