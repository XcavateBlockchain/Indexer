//! Row shapes and slot-guarded writes for the `property` program's account-state tables
//! (`migrations/0010_property_state.sql`). Same contract as [`super::accounts`].
//!
//! Two shape notes (see the migration header): `LettingAgent.locations` is stored as JSONB in
//! a shape this indexer constructs (postcodes as UTF-8 strings -- NOT the decoder's serde
//! byte arrays; the conditional close below compares against it), and
//! `PropertyLetting.election` is flattened into `election_*` columns. `PropertyLetting.agent`
//! keeps the on-chain all-zero-pubkey "seat vacant" sentinel verbatim.

use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

#[derive(Debug, Clone)]
pub struct PropertyConfigRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub authority: Vec<u8>,
    pub pending_authority: Option<Vec<u8>>,
    pub xcav_mint: Vec<u8>,
    pub treasury: Vec<u8>,
    pub rent_collector: Vec<u8>,
    pub agent_deposit: i64,
    pub agent_voting_time: i64,
    pub min_voting_quorum_bps: i32,
    pub agent_notice_period: i64,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct AgentCandidacyRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub asset_id: i64,
    pub round: i64,
    pub agent: Vec<u8>,
    pub vote_power: i64,
    pub rent_payer: Vec<u8>,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct AgentVoteRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub asset_id: i64,
    pub round: i64,
    pub voter: Vec<u8>,
    pub choice: Vec<u8>,
    pub power: i64,
    pub rent_payer: Vec<u8>,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct LettingAgentRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub wallet: Vec<u8>,
    pub region_id: i32,
    /// The on-chain `Vec<AgentLocation>` in the shape this indexer constructs (see
    /// migration 0010): `[{"postcode": "E14", "assigned_count": 0, "deposit": 1000000}, ...]`
    /// -- postcodes as UTF-8 strings, which the conditional close below compares against.
    pub locations: serde_json::Value,
    pub rent_payer: Vec<u8>,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct PropertyLettingRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub asset_id: i64,
    /// All-zero pubkey = seat vacant (on-chain sentinel, stored verbatim).
    pub agent: Vec<u8>,
    pub election_expiry: i64,
    pub election_candidate_count: i64,
    pub election_round: i64,
    pub election_quorum_bps: i32,
    pub rent_payer: Vec<u8>,
    pub bump: i16,
}

#[derive(Debug, Clone)]
pub struct ResignationNoticeRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub asset_id: i64,
    pub agent: Vec<u8>,
    pub due_ts: i64,
    pub rent_payer: Vec<u8>,
    pub bump: i16,
}

/// One decoded property account, ready to upsert.
#[derive(Debug, Clone)]
pub enum PropertyAccountRow {
    Config(PropertyConfigRow),
    AgentCandidacy(AgentCandidacyRow),
    AgentVote(AgentVoteRow),
    LettingAgent(LettingAgentRow),
    PropertyLetting(PropertyLettingRow),
    ResignationNotice(ResignationNoticeRow),
}

impl PropertyAccountRow {
    pub fn slot(&self) -> i64 {
        match self {
            PropertyAccountRow::Config(r) => r.slot,
            PropertyAccountRow::AgentCandidacy(r) => r.slot,
            PropertyAccountRow::AgentVote(r) => r.slot,
            PropertyAccountRow::LettingAgent(r) => r.slot,
            PropertyAccountRow::PropertyLetting(r) => r.slot,
            PropertyAccountRow::ResignationNotice(r) => r.slot,
        }
    }
}

/// Dispatch one decoded property account to its table's slot-guarded upsert.
pub async fn upsert<'e, E>(
    executor: E,
    row: &PropertyAccountRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    match row {
        PropertyAccountRow::Config(r) => upsert_config(executor, r).await,
        PropertyAccountRow::AgentCandidacy(r) => upsert_agent_candidacy(executor, r).await,
        PropertyAccountRow::AgentVote(r) => upsert_agent_vote(executor, r).await,
        PropertyAccountRow::LettingAgent(r) => upsert_letting_agent(executor, r).await,
        PropertyAccountRow::PropertyLetting(r) => upsert_property_letting(executor, r).await,
        PropertyAccountRow::ResignationNotice(r) => upsert_resignation_notice(executor, r).await,
    }
}

pub async fn upsert_config<'e, E>(
    executor: E,
    row: &PropertyConfigRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO property_config (pubkey, slot, lamports, closed_at_slot, authority,
            pending_authority, xcav_mint, treasury, rent_collector, agent_deposit,
            agent_voting_time, min_voting_quorum_bps, agent_notice_period, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot                  = EXCLUDED.slot,
            lamports              = EXCLUDED.lamports,
            closed_at_slot        = EXCLUDED.closed_at_slot,
            authority             = EXCLUDED.authority,
            pending_authority     = EXCLUDED.pending_authority,
            xcav_mint             = EXCLUDED.xcav_mint,
            treasury              = EXCLUDED.treasury,
            rent_collector        = EXCLUDED.rent_collector,
            agent_deposit         = EXCLUDED.agent_deposit,
            agent_voting_time     = EXCLUDED.agent_voting_time,
            min_voting_quorum_bps = EXCLUDED.min_voting_quorum_bps,
            agent_notice_period   = EXCLUDED.agent_notice_period,
            bump                  = EXCLUDED.bump
        WHERE property_config.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.authority,
        row.pending_authority.as_deref(),
        row.xcav_mint,
        row.treasury,
        row.rent_collector,
        row.agent_deposit,
        row.agent_voting_time,
        row.min_voting_quorum_bps,
        row.agent_notice_period,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_agent_candidacy<'e, E>(
    executor: E,
    row: &AgentCandidacyRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO property_agent_candidacy (pubkey, slot, lamports, closed_at_slot, asset_id,
            round, agent, vote_power, rent_payer, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            asset_id       = EXCLUDED.asset_id,
            round          = EXCLUDED.round,
            agent          = EXCLUDED.agent,
            vote_power     = EXCLUDED.vote_power,
            rent_payer     = EXCLUDED.rent_payer,
            bump           = EXCLUDED.bump
        WHERE property_agent_candidacy.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.asset_id,
        row.round,
        row.agent,
        row.vote_power,
        row.rent_payer,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_agent_vote<'e, E>(
    executor: E,
    row: &AgentVoteRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO property_agent_vote (pubkey, slot, lamports, closed_at_slot, asset_id,
            round, voter, choice, power, rent_payer, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            asset_id       = EXCLUDED.asset_id,
            round          = EXCLUDED.round,
            voter          = EXCLUDED.voter,
            choice         = EXCLUDED.choice,
            power          = EXCLUDED.power,
            rent_payer     = EXCLUDED.rent_payer,
            bump           = EXCLUDED.bump
        WHERE property_agent_vote.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.asset_id,
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

pub async fn upsert_letting_agent<'e, E>(
    executor: E,
    row: &LettingAgentRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO property_letting_agent (pubkey, slot, lamports, closed_at_slot, wallet,
            region_id, locations, rent_payer, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            wallet         = EXCLUDED.wallet,
            region_id      = EXCLUDED.region_id,
            locations      = EXCLUDED.locations,
            rent_payer     = EXCLUDED.rent_payer,
            bump           = EXCLUDED.bump
        WHERE property_letting_agent.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.wallet,
        row.region_id,
        row.locations,
        row.rent_payer,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_property_letting<'e, E>(
    executor: E,
    row: &PropertyLettingRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO property_letting (pubkey, slot, lamports, closed_at_slot, asset_id, agent,
            election_expiry, election_candidate_count, election_round, election_quorum_bps,
            rent_payer, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot                     = EXCLUDED.slot,
            lamports                 = EXCLUDED.lamports,
            closed_at_slot           = EXCLUDED.closed_at_slot,
            asset_id                 = EXCLUDED.asset_id,
            agent                    = EXCLUDED.agent,
            election_expiry          = EXCLUDED.election_expiry,
            election_candidate_count = EXCLUDED.election_candidate_count,
            election_round           = EXCLUDED.election_round,
            election_quorum_bps      = EXCLUDED.election_quorum_bps,
            rent_payer               = EXCLUDED.rent_payer,
            bump                     = EXCLUDED.bump
        WHERE property_letting.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.asset_id,
        row.agent,
        row.election_expiry,
        row.election_candidate_count,
        row.election_round,
        row.election_quorum_bps,
        row.rent_payer,
        row.bump,
    )
    .execute(executor)
    .await
}

pub async fn upsert_resignation_notice<'e, E>(
    executor: E,
    row: &ResignationNoticeRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO property_resignation_notice (pubkey, slot, lamports, closed_at_slot,
            asset_id, agent, due_ts, rent_payer, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot           = EXCLUDED.slot,
            lamports       = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            asset_id       = EXCLUDED.asset_id,
            agent          = EXCLUDED.agent,
            due_ts         = EXCLUDED.due_ts,
            rent_payer     = EXCLUDED.rent_payer,
            bump           = EXCLUDED.bump
        WHERE property_resignation_notice.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.asset_id,
        row.agent,
        row.due_ts,
        row.rent_payer,
        row.bump,
    )
    .execute(executor)
    .await
}

/// The conditional close for `remove_letting_agent` (the one close in the protocol that is a
/// runtime `close()` call rather than an Anchor `close =` constraint): on-chain, the
/// `LettingAgent` PDA is closed only when the removed location was its LAST one. The mapper
/// is pure (no DB access), so the decision is made here, where the stored row is available:
/// close only if the stored pre-instruction row has exactly one location left and it is the
/// one this instruction removed.
///
/// Both conditions are belt-and-braces on top of the slot guard: for a successful
/// transaction (failed ones are never indexed), the on-chain pre-state had the removed
/// postcode present, so a fresh stored row with one location IS the on-chain "last location"
/// case. A stale stored row (missed intermediate updates during a stream gap) can
/// mis-decide in either direction. A wrong close is NOT healed by the remove instruction's
/// own post-state account update -- that update carries the same slot as this close, which
/// the strict upsert guard rejects -- so the healing paths are any later write to the
/// account and, definitively, the snapshot sweep (`close_missing_in_table` closes what is
/// truly gone; a snapshot upsert at a newer slot revives what is truly live).
pub async fn close_letting_agent_if_last<'e, E>(
    executor: E,
    pubkey: &[u8],
    removed_postcode: &serde_json::Value,
    slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        UPDATE property_letting_agent
        SET slot = $2, closed_at_slot = $2
        WHERE pubkey = $1
          AND slot < $2
          AND jsonb_array_length(locations) = 1
          AND locations -> 0 -> 'postcode' = $3
        "#,
        pubkey,
        slot,
        removed_postcode,
    )
    .execute(executor)
    .await
}
