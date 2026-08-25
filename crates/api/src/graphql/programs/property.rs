//! The `property` program's read surface: entity types over `migrations/0010_property_state.sql`
//! (as extended by `0012_redeploy_new_programs.sql`) and the resolver bodies `QueryRoot`
//! delegates to. See [`super`] for the shared conventions.

use carbon_core::graphql::primitives::I64;
use juniper::{FieldResult, GraphQLObject, ID};

use super::{b58, hex_string, json_string, parse_b58, total_count_i32};
use crate::graphql::context::GraphQLContext;
use crate::graphql::enums::{unknown_enum_value, GovVoteChoice};
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
    pub proposal_voting_time: I64,
    pub low_proposal: I64,
    pub high_proposal: I64,
    pub high_threshold_bps: i32,
    pub auto_approval_cooldown: I64,
    pub challenge_deposit: I64,
    pub agent_slash_amount: I64,
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
    pub governance_proposal_count: I64,
    pub governance_challenge_count: I64,
    pub governance_active_proposal: I64,
    pub governance_active_challenge: I64,
    pub governance_strikes: i32,
    pub governance_last_auto_approval_ts: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyLettingConnection {
    pub nodes: Vec<PropertyLetting>,
    pub total_count: i32,
}

/// A spending proposal put to a property's holders (only above-low-tier proposals reach
/// storage; auto-approved ones close inside their own instruction). `detailsHash` is the
/// off-chain document hash, lowercase hex.
#[derive(GraphQLObject, Clone, Debug)]
pub struct Proposal {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub proposal_id: I64,
    pub proposer: String,
    pub amount: I64,
    pub details_hash: String,
    pub expiry: I64,
    pub tally_yes: I64,
    pub tally_no: I64,
    pub tally_abstain: I64,
    pub quorum_bps: i32,
    pub threshold_bps: i32,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ProposalConnection {
    pub nodes: Vec<Proposal>,
    pub total_count: i32,
}

/// A holder's move to strike the sitting agent, backed by an XCAV stake.
#[derive(GraphQLObject, Clone, Debug)]
pub struct Challenge {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub challenge_id: I64,
    pub challenger: String,
    pub agent: String,
    pub deposit: I64,
    pub expiry: I64,
    pub tally_yes: I64,
    pub tally_no: I64,
    pub tally_abstain: I64,
    pub quorum_bps: i32,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct ChallengeConnection {
    pub nodes: Vec<Challenge>,
    pub total_count: i32,
}

/// One holder's vote on one proposal OR challenge -- the account data does not say which
/// (only the on-chain seed prefix differs); join `voteId` against proposals/challenges to
/// disambiguate (see migration 0012).
#[derive(GraphQLObject, Clone, Debug)]
pub struct GovVote {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub vote_id: I64,
    pub voter: String,
    pub choice: GovVoteChoice,
    pub power: I64,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct GovVoteConnection {
    pub nodes: Vec<GovVote>,
    pub total_count: i32,
}

/// A property's rental income ledger. `streams` is the raw JSON
/// `[{"mint": "<base58>", "per_share": "<decimal string>", "dust": N}, ...]` (see migration
/// 0012; `per_share` is u128 on-chain, carried as a decimal string).
#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyIncome {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub streams: String,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct PropertyIncomeConnection {
    pub nodes: Vec<PropertyIncome>,
    pub total_count: i32,
}

/// One holder's income claim state on one property. `entries` is the raw JSON
/// `[{"per_share": "<decimal string>", "pending": N}, ...]`, entries[i] tracking the
/// income ledger's streams[i].
#[derive(GraphQLObject, Clone, Debug)]
pub struct IncomeCheckpoint {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub asset_id: I64,
    pub owner: String,
    pub entries: String,
    pub rent_payer: String,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct IncomeCheckpointConnection {
    pub nodes: Vec<IncomeCheckpoint>,
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
               min_voting_quorum_bps, agent_notice_period, proposal_voting_time, low_proposal,
               high_proposal, high_threshold_bps, auto_approval_cooldown, challenge_deposit,
               agent_slash_amount
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
        proposal_voting_time: I64(r.proposal_voting_time),
        low_proposal: I64(r.low_proposal),
        high_proposal: I64(r.high_proposal),
        high_threshold_bps: r.high_threshold_bps,
        auto_approval_cooldown: I64(r.auto_approval_cooldown),
        challenge_deposit: I64(r.challenge_deposit),
        agent_slash_amount: I64(r.agent_slash_amount),
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
               election_candidate_count, election_round, election_quorum_bps,
               governance_proposal_count, governance_challenge_count,
               governance_active_proposal, governance_active_challenge, governance_strikes,
               governance_last_auto_approval_ts, rent_payer
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
                governance_proposal_count: I64(r.governance_proposal_count),
                governance_challenge_count: I64(r.governance_challenge_count),
                governance_active_proposal: I64(r.governance_active_proposal),
                governance_active_challenge: I64(r.governance_active_challenge),
                governance_strikes: r.governance_strikes as i32,
                governance_last_auto_approval_ts: I64(r.governance_last_auto_approval_ts),
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

pub async fn proposals(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    proposer: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ProposalConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let proposer = proposer
        .as_deref()
        .map(|s| parse_b58("proposer", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, id, proposer, amount,
               details_hash, expiry, tally_yes, tally_no, tally_abstain, quorum_bps,
               threshold_bps, rent_payer
        FROM property_proposal
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR proposer = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        proposer.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM property_proposal
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR proposer = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        proposer.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(ProposalConnection {
        nodes: rows
            .into_iter()
            .map(|r| Proposal {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                proposal_id: I64(r.id),
                proposer: b58(&r.proposer),
                amount: I64(r.amount),
                details_hash: hex_string(&r.details_hash),
                expiry: I64(r.expiry),
                tally_yes: I64(r.tally_yes),
                tally_no: I64(r.tally_no),
                tally_abstain: I64(r.tally_abstain),
                quorum_bps: r.quorum_bps,
                threshold_bps: r.threshold_bps,
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn challenges(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    challenger: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<ChallengeConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let challenger = challenger
        .as_deref()
        .map(|s| parse_b58("challenger", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, id, challenger, agent,
               deposit, expiry, tally_yes, tally_no, tally_abstain, quorum_bps, rent_payer
        FROM property_challenge
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR challenger = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $4 OFFSET $5
        "#,
        asset_id,
        challenger.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM property_challenge
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bytea IS NULL OR challenger = $2)
          AND ($3::bool IS NULL OR (closed_at_slot IS NULL) = $3)
        "#,
        asset_id,
        challenger.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(ChallengeConnection {
        nodes: rows
            .into_iter()
            .map(|r| Challenge {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                challenge_id: I64(r.id),
                challenger: b58(&r.challenger),
                agent: b58(&r.agent),
                deposit: I64(r.deposit),
                expiry: I64(r.expiry),
                tally_yes: I64(r.tally_yes),
                tally_no: I64(r.tally_no),
                tally_abstain: I64(r.tally_abstain),
                quorum_bps: r.quorum_bps,
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn gov_votes(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    voter: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<GovVoteConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let voter = voter
        .as_deref()
        .map(|s| parse_b58("voter", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, id, voter, choice, power,
               rent_payer
        FROM property_gov_vote
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
        SELECT count(*) FROM property_gov_vote
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

    let nodes = rows
        .into_iter()
        .map(|r| {
            let choice = GovVoteChoice::from_db_str(&r.choice)
                .ok_or_else(|| unknown_enum_value("choice", &r.choice))?;
            Ok(GovVote {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                vote_id: I64(r.id),
                voter: b58(&r.voter),
                choice,
                power: I64(r.power),
                rent_payer: b58(&r.rent_payer),
            })
        })
        .collect::<FieldResult<Vec<_>>>()?;

    Ok(GovVoteConnection {
        nodes,
        total_count: total_count_i32(total),
    })
}

pub async fn property_incomes(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<PropertyIncomeConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, streams, rent_payer
        FROM property_income
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $3 OFFSET $4
        "#,
        asset_id,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM property_income
        WHERE ($1::bigint IS NULL OR asset_id = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        "#,
        asset_id,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(PropertyIncomeConnection {
        nodes: rows
            .into_iter()
            .map(|r| PropertyIncome {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                streams: json_string(&r.streams),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn income_checkpoints(
    context: &GraphQLContext,
    asset_id: Option<I64>,
    owner: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<IncomeCheckpointConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let asset_id = asset_id.map(|v| v.0);
    let owner = owner
        .as_deref()
        .map(|s| parse_b58("owner", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, asset_id, owner, entries, rent_payer
        FROM property_income_checkpoint
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
        SELECT count(*) FROM property_income_checkpoint
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

    Ok(IncomeCheckpointConnection {
        nodes: rows
            .into_iter()
            .map(|r| IncomeCheckpoint {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                asset_id: I64(r.asset_id),
                owner: b58(&r.owner),
                entries: json_string(&r.entries),
                rent_payer: b58(&r.rent_payer),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}
