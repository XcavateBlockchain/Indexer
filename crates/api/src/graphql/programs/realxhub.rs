//! The `realxhub` program's read surface: entity types over `migrations/0015_realxhub_state.sql`
//! and the resolver bodies `QueryRoot` delegates to. See [`super`] for the shared conventions.
//!
//! u128 on-chain fields (`hub.income_per_share`, `holding.per_share`) are stored as decimal
//! TEXT and surfaced raw as `String`, mirroring the house "raw text → String" convention.
//! The `bump` columns are part of the canonical state snapshot in the database but are not
//! exposed here, matching the sibling programs.

use carbon_core::graphql::primitives::I64;
use juniper::{FieldResult, GraphQLObject, ID};

use super::{b58, parse_b58, total_count_i32};
use crate::graphql::context::GraphQLContext;
use crate::guards::{clamp_first, clamp_offset};

/// The realxhub program's Config PDA (singleton). `null` until `initialize` has been indexed.
#[derive(GraphQLObject, Clone, Debug)]
pub struct RealxhubConfig {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub authority: String,
    pub stable_mint: String,
    pub next_hub_id: I64,
}

/// A Hub PDA: one created hub with its share mint, the revenue-split wallets and its
/// cumulative income accounting. `hub_id` is the on-chain `Hub.id` (the PDA seed); `id` is
/// the account pubkey.
#[derive(GraphQLObject, Clone, Debug)]
pub struct RealxhubHub {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub hub_id: I64,
    pub name: String,
    pub share_mint: String,
    pub operational_spv: String,
    pub supplier: String,
    pub operators: String,
    pub protocol: String,
    pub per_wallet_cap: I64,
    pub income_per_share: String,
    pub income_dust: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct RealxhubHubConnection {
    pub nodes: Vec<RealxhubHub>,
    pub total_count: i32,
}

/// A Holding PDA: the canonical per-holder share ledger. `owner` is the holder's wallet as
/// embedded in the on-chain state and `hub_id` the on-chain hub index (ADR-30 addendum
/// 2026-09-03); listings/holdings still pair on the PDA pubkey.
#[derive(GraphQLObject, Clone, Debug)]
pub struct RealxhubHolding {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub hub_id: I64,
    pub owner: String,
    pub amount: I64,
    pub listed: I64,
    pub per_share: String,
    pub pending: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct RealxhubHoldingConnection {
    pub nodes: Vec<RealxhubHolding>,
    pub total_count: i32,
}

/// A ShareListing PDA: one seller's live listing for a hub (delist closes the account; the
/// same address can be re-listed later and show up again). `hub_id` is the on-chain hub index
/// (ADR-30 addendum 2026-09-03).
#[derive(GraphQLObject, Clone, Debug)]
pub struct RealxhubShareListing {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub hub_id: I64,
    pub seller: String,
    pub amount: I64,
    pub price: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct RealxhubShareListingConnection {
    pub nodes: Vec<RealxhubShareListing>,
    pub total_count: i32,
}

/// A FaucetReceipt PDA: per-wallet faucet cooldown marker.
#[derive(GraphQLObject, Clone, Debug)]
pub struct RealxhubFaucetReceipt {
    pub id: ID,
    pub slot: I64,
    pub lamports: I64,
    pub active: bool,
    pub closed_at_slot: Option<I64>,
    pub last_drip: I64,
}

#[derive(GraphQLObject, Clone, Debug)]
pub struct RealxhubFaucetReceiptConnection {
    pub nodes: Vec<RealxhubFaucetReceipt>,
    pub total_count: i32,
}

// --- resolver bodies ------------------------------------------------------------------------

pub async fn realxhub_config(context: &GraphQLContext) -> FieldResult<Option<RealxhubConfig>> {
    let row = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, authority, stable_mint, next_hub_id
        FROM realxhub_config
        ORDER BY slot DESC
        LIMIT 1
        "#
    )
    .fetch_optional(&context.pool)
    .await?;
    Ok(row.map(|r| RealxhubConfig {
        id: ID::new(b58(&r.pubkey)),
        slot: I64(r.slot),
        lamports: I64(r.lamports),
        active: r.closed_at_slot.is_none(),
        closed_at_slot: r.closed_at_slot.map(I64),
        authority: b58(&r.authority),
        stable_mint: b58(&r.stable_mint),
        next_hub_id: I64(r.next_hub_id),
    }))
}

pub async fn realxhub_hubs(
    context: &GraphQLContext,
    hub_id: Option<I64>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<RealxhubHubConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let hub_id = hub_id.map(|v| v.0);

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, id, name, share_mint, operational_spv,
               supplier, operators, protocol, per_wallet_cap, income_per_share, income_dust
        FROM realxhub_hub
        WHERE ($1::bigint IS NULL OR id = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $3 OFFSET $4
        "#,
        hub_id,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM realxhub_hub
        WHERE ($1::bigint IS NULL OR id = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        "#,
        hub_id,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(RealxhubHubConnection {
        nodes: rows
            .into_iter()
            .map(|r| RealxhubHub {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                hub_id: I64(r.id),
                name: r.name,
                share_mint: b58(&r.share_mint),
                operational_spv: b58(&r.operational_spv),
                supplier: b58(&r.supplier),
                operators: b58(&r.operators),
                protocol: b58(&r.protocol),
                per_wallet_cap: I64(r.per_wallet_cap),
                income_per_share: r.income_per_share,
                income_dust: I64(r.income_dust),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn realxhub_holdings(
    context: &GraphQLContext,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<RealxhubHoldingConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, hub_id, owner, amount, listed, per_share, pending
        FROM realxhub_holding
        WHERE ($1::bool IS NULL OR (closed_at_slot IS NULL) = $1)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $2 OFFSET $3
        "#,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM realxhub_holding
        WHERE ($1::bool IS NULL OR (closed_at_slot IS NULL) = $1)
        "#,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(RealxhubHoldingConnection {
        nodes: rows
            .into_iter()
            .map(|r| RealxhubHolding {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                hub_id: I64(r.hub_id),
                owner: b58(&r.owner),
                amount: I64(r.amount),
                listed: I64(r.listed),
                per_share: r.per_share,
                pending: I64(r.pending),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn realxhub_share_listings(
    context: &GraphQLContext,
    seller: Option<String>,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<RealxhubShareListingConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);
    let seller = seller
        .as_deref()
        .map(|s| parse_b58("seller", s))
        .transpose()?;

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, hub_id, seller, amount, price
        FROM realxhub_share_listing
        WHERE ($1::bytea IS NULL OR seller = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $3 OFFSET $4
        "#,
        seller.as_deref(),
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM realxhub_share_listing
        WHERE ($1::bytea IS NULL OR seller = $1)
          AND ($2::bool IS NULL OR (closed_at_slot IS NULL) = $2)
        "#,
        seller.as_deref(),
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(RealxhubShareListingConnection {
        nodes: rows
            .into_iter()
            .map(|r| RealxhubShareListing {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                hub_id: I64(r.hub_id),
                seller: b58(&r.seller),
                amount: I64(r.amount),
                price: I64(r.price),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}

pub async fn realxhub_faucet_receipts(
    context: &GraphQLContext,
    active: Option<bool>,
    first: Option<i32>,
    offset: Option<i32>,
) -> FieldResult<RealxhubFaucetReceiptConnection> {
    let limit = clamp_first(first);
    let skip = clamp_offset(offset);

    let rows = sqlx::query!(
        r#"
        SELECT pubkey, slot, lamports, closed_at_slot, last_drip
        FROM realxhub_faucet_receipt
        WHERE ($1::bool IS NULL OR (closed_at_slot IS NULL) = $1)
        ORDER BY slot DESC, pubkey ASC
        LIMIT $2 OFFSET $3
        "#,
        active,
        limit,
        skip,
    )
    .fetch_all(&context.pool)
    .await?;

    let total = sqlx::query_scalar!(
        r#"
        SELECT count(*) FROM realxhub_faucet_receipt
        WHERE ($1::bool IS NULL OR (closed_at_slot IS NULL) = $1)
        "#,
        active,
    )
    .fetch_one(&context.pool)
    .await?
    .unwrap_or(0);

    Ok(RealxhubFaucetReceiptConnection {
        nodes: rows
            .into_iter()
            .map(|r| RealxhubFaucetReceipt {
                id: ID::new(b58(&r.pubkey)),
                slot: I64(r.slot),
                lamports: I64(r.lamports),
                active: r.closed_at_slot.is_none(),
                closed_at_slot: r.closed_at_slot.map(I64),
                last_drip: I64(r.last_drip),
            })
            .collect(),
        total_count: total_count_i32(total),
    })
}
