//! Row shapes and slot-guarded writes for the `realxhub` program's account-state
//! tables (`migrations/0015_realxhub_state.sql`). Same contract as
//! [`super::accounts`]: every upsert is `ON CONFLICT (pubkey) DO UPDATE ...
//! WHERE t.slot < EXCLUDED.slot`, so an out-of-order delivery can never move an
//! account backwards, and `closed_at_slot` is always written together with
//! `slot` (see [`super::close`]).
//!
//! realxhub (the realXhub demo: fractional hub shares on a secondary market,
//! ADR-30) keeps its state in five PDAs, one per table here:
//!
//! - [`RealxhubConfigRow`] — the single `["config"]` PDA: authority, the
//!   stable mint, and the `next_hub_id` counter that names the hub PDAs.
//! - [`RealxhubFaucetReceiptRow`] — the per-wallet `["faucet", wallet]`
//!   cooldown marker (`last_drip` is unix time).
//! - [`RealxhubHoldingRow`] — the canonical `["holding", hub_id, wallet]`
//!   ledger: held amount, the listed subset, and the pending income.
//! - [`RealxhubHubRow`] — the `["hub", hub_id]` account: name, the five
//!   economic roles, the per-wallet cap, and the cumulative per-share income.
//! - [`RealxhubShareListingRow`] — the `["listing", hub_id, seller]` account:
//!   the shares on sale and their stablecoin price per share.
//!
//! Type mapping follows the house convention (see the migration header): the
//! `u128` fields (`Hub.income_per_share`, `Holding.per_share`) are decimal
//! TEXT columns, every other integer fits in `BIGINT`, `bump` is `SMALLINT`.
//!
//! The `realxhub_share_listing` table also gets a conditional close
//! ([`close_share_listing_if_emptied`]) mirroring
//! [`super::marketplace::close_share_listing_if_emptied`]: when the last
//! listed share is bought, the buy instruction both writes the new (zero)
//! listing row and closes it in the same slot, leaving a stable final state.

use sqlx::postgres::PgQueryResult;
use sqlx::PgExecutor;

/// Row for the `realxhub` program's single `Config` account (PDA `["config"]`).
#[derive(Debug, Clone)]
pub struct RealxhubConfigRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub authority: Vec<u8>,
    pub stable_mint: Vec<u8>,
    pub next_hub_id: i64,
    pub bump: i16,
}

/// Row for a `realxhub` `FaucetReceipt` account (PDA `["faucet", wallet]`).
#[derive(Debug, Clone)]
pub struct RealxhubFaucetReceiptRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub last_drip: i64,
    pub bump: i16,
}

/// Row for a `realxhub` `Holding` account (PDA `["holding", hub_id, wallet]`).
/// The canonical per-holder ledger; the share tokens are frozen and this
/// account is what the program trusts (ADR-30).
#[derive(Debug, Clone)]
pub struct RealxhubHoldingRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub amount: i64,
    pub listed: i64,
    /// `per_share`: the hub's `income_per_share` this holding is settled up to.
    pub per_share: String,
    /// `pending`: settled income not yet paid out.
    pub pending: i64,
    pub bump: i16,
}

/// Row for a `realxhub` `Hub` account (PDA `["hub", hub_id]`). The hub PDA
/// doubles as the share mint's freeze authority and permanent delegate.
#[derive(Debug, Clone)]
pub struct RealxhubHubRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub id: i64,
    pub name: String,
    pub share_mint: Vec<u8>,
    /// `operational_spv`: first holder of all 100 shares.
    pub operational_spv: Vec<u8>,
    pub supplier: Vec<u8>,
    pub operators: Vec<u8>,
    pub protocol: Vec<u8>,
    pub per_wallet_cap: i64,
    /// Cumulative stablecoin income per share since the hub was created.
    pub income_per_share: String,
    /// Remainder of holder legs that didn't divide by the supply.
    pub income_dust: i64,
    pub bump: i16,
}

/// Row for a `realxhub` `ShareListing` account
/// (PDA `["listing", hub_id, seller]`). One live listing per seller per hub;
/// `delist_shares` closes the account.
#[derive(Debug, Clone)]
pub struct RealxhubShareListingRow {
    pub pubkey: Vec<u8>,
    pub slot: i64,
    pub lamports: i64,
    pub seller: Vec<u8>,
    pub amount: i64,
    /// Stablecoin per share.
    pub price: i64,
    pub bump: i16,
}

/// Every `realxhub` account-state row the upsert dispatch can target.
#[derive(Debug, Clone)]
pub enum RealxhubAccountRow {
    Config(RealxhubConfigRow),
    FaucetReceipt(RealxhubFaucetReceiptRow),
    Holding(RealxhubHoldingRow),
    Hub(RealxhubHubRow),
    ShareListing(RealxhubShareListingRow),
}

impl RealxhubAccountRow {
    /// The slot this row describes — the batcher orders writes by it, so
    /// every variant must agree.
    pub fn slot(&self) -> i64 {
        match self {
            RealxhubAccountRow::Config(r) => r.slot,
            RealxhubAccountRow::FaucetReceipt(r) => r.slot,
            RealxhubAccountRow::Holding(r) => r.slot,
            RealxhubAccountRow::Hub(r) => r.slot,
            RealxhubAccountRow::ShareListing(r) => r.slot,
        }
    }
}

/// Slot-guarded upsert for the single `Config` PDA.
pub async fn upsert_config<'e, E>(
    executor: E,
    row: &RealxhubConfigRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO realxhub_config (pubkey, slot, lamports, closed_at_slot, authority, stable_mint, next_hub_id, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot = EXCLUDED.slot,
            lamports = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            authority = EXCLUDED.authority,
            stable_mint = EXCLUDED.stable_mint,
            next_hub_id = EXCLUDED.next_hub_id,
            bump = EXCLUDED.bump
        WHERE realxhub_config.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.authority,
        row.stable_mint,
        row.next_hub_id,
        row.bump,
    )
    .execute(executor)
    .await
}

/// Slot-guarded upsert for a `FaucetReceipt` account (per-wallet faucet
/// cooldown marker).
pub async fn upsert_faucet_receipt<'e, E>(
    executor: E,
    row: &RealxhubFaucetReceiptRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO realxhub_faucet_receipt (pubkey, slot, lamports, closed_at_slot, last_drip, bump)
        VALUES ($1, $2, $3, NULL, $4, $5)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot = EXCLUDED.slot,
            lamports = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            last_drip = EXCLUDED.last_drip,
            bump = EXCLUDED.bump
        WHERE realxhub_faucet_receipt.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.last_drip,
        row.bump,
    )
    .execute(executor)
    .await
}

/// Slot-guarded upsert for a `Holding` account (the canonical per-holder
/// ledger).
pub async fn upsert_holding<'e, E>(
    executor: E,
    row: &RealxhubHoldingRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO realxhub_holding (pubkey, slot, lamports, closed_at_slot, amount, listed, per_share, pending, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot = EXCLUDED.slot,
            lamports = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            amount = EXCLUDED.amount,
            listed = EXCLUDED.listed,
            per_share = EXCLUDED.per_share,
            pending = EXCLUDED.pending,
            bump = EXCLUDED.bump
        WHERE realxhub_holding.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.amount,
        row.listed,
        row.per_share,
        row.pending,
        row.bump,
    )
    .execute(executor)
    .await
}

/// Slot-guarded upsert for a `Hub` account. The u128 fields
/// (`income_per_share`) travel as decimal strings (ADR-30).
pub async fn upsert_hub<'e, E>(
    executor: E,
    row: &RealxhubHubRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO realxhub_hub (pubkey, slot, lamports, closed_at_slot, id, name, share_mint, operational_spv, supplier, operators, protocol, per_wallet_cap, income_per_share, income_dust, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot = EXCLUDED.slot,
            lamports = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            id = EXCLUDED.id,
            name = EXCLUDED.name,
            share_mint = EXCLUDED.share_mint,
            operational_spv = EXCLUDED.operational_spv,
            supplier = EXCLUDED.supplier,
            operators = EXCLUDED.operators,
            protocol = EXCLUDED.protocol,
            per_wallet_cap = EXCLUDED.per_wallet_cap,
            income_per_share = EXCLUDED.income_per_share,
            income_dust = EXCLUDED.income_dust,
            bump = EXCLUDED.bump
        WHERE realxhub_hub.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.id,
        row.name,
        row.share_mint,
        row.operational_spv,
        row.supplier,
        row.operators,
        row.protocol,
        row.per_wallet_cap,
        row.income_per_share,
        row.income_dust,
        row.bump,
    )
    .execute(executor)
    .await
}

/// Slot-guarded upsert for a `ShareListing` account (one live listing per
/// seller per hub).
pub async fn upsert_share_listing<'e, E>(
    executor: E,
    row: &RealxhubShareListingRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        INSERT INTO realxhub_share_listing (pubkey, slot, lamports, closed_at_slot, seller, amount, price, bump)
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7)
        ON CONFLICT (pubkey) DO UPDATE SET
            slot = EXCLUDED.slot,
            lamports = EXCLUDED.lamports,
            closed_at_slot = EXCLUDED.closed_at_slot,
            seller = EXCLUDED.seller,
            amount = EXCLUDED.amount,
            price = EXCLUDED.price,
            bump = EXCLUDED.bump
        WHERE realxhub_share_listing.slot < EXCLUDED.slot
        "#,
        row.pubkey,
        row.slot,
        row.lamports,
        row.seller,
        row.amount,
        row.price,
        row.bump,
    )
    .execute(executor)
    .await
}

/// Dispatch a slot-guarded upsert to the right `realxhub` table.
pub async fn upsert<'e, E>(
    executor: E,
    row: &RealxhubAccountRow,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    match row {
        RealxhubAccountRow::Config(r) => upsert_config(executor, r).await,
        RealxhubAccountRow::FaucetReceipt(r) => upsert_faucet_receipt(executor, r).await,
        RealxhubAccountRow::Holding(r) => upsert_holding(executor, r).await,
        RealxhubAccountRow::Hub(r) => upsert_hub(executor, r).await,
        RealxhubAccountRow::ShareListing(r) => upsert_share_listing(executor, r).await,
    }
}

/// Conditional close for `buy_shares`: mark `realxhub_share_listing` closed
/// in `slot` **only if it still holds exactly `bought_amount` shares** —
/// i.e. the buy emptied it.
///
/// The mapper is pure and cannot look up the stored pre-buy state, so the
/// "did the listing empty?" decision is made here, against the row the
/// database still has for that pubkey. Same caveats as
/// [`super::marketplace::close_share_listing_if_emptied`]: a same-slot
/// rewrite by another `buy_shares` for the same listing (impossible here —
/// one listing is one PDA and buys are serialized by its state) is a no-op,
/// and out-of-order delivery is healed by the `slot <` guard. The healed
/// listing is a terminal state, like any closed account: `active` reads
/// exclude it until an explicit `list_shares` re-creates it.
pub async fn close_share_listing_if_emptied<'e, E>(
    executor: E,
    pubkey: &[u8],
    bought_amount: i64,
    slot: i64,
) -> Result<PgQueryResult, sqlx::Error>
where
    E: PgExecutor<'e>,
{
    sqlx::query!(
        r#"
        UPDATE realxhub_share_listing
        SET slot = $2,
            closed_at_slot = $2
        WHERE pubkey = $1
          AND slot < $2
          AND amount = $3
        "#,
        pubkey,
        slot,
        bought_amount
    )
    .execute(executor)
    .await
}
