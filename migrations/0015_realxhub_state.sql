-- Migration 0015 — realxhub account state tables.
--
-- Account-state tables for the `realxhub` program (Hjd9AHDefWgTnwCGLCoTPRqttpXHWCAUE3zv9aeNJnXu),
-- following 0002's slot-guarded pattern; see 0008's header for the shared conventions
-- (integer widths, soft closes, revive-on-newer-slot, snake_case `realxhub_` prefix). The
-- program is not yet deployed on devnet (ADR-30): it arrives ahead of the chain, so the
-- indexer accepts its events once the program lands and deploy_slot 0 is replaced by the
-- real first-slot.
--
-- Shape decisions specific to this program:
--
--   * `realxhub_config` / `realxhub_faucet_receipt` / `realxhub_holding` /
--     `realxhub_hub` / `realxhub_share_listing`: one per account, mirroring the
--     `marketplace_` family shape from 0009/0012. `closed_at_slot` stays NULL forever for
--     config/faucet_receipt/holding/hub (no close instruction in the IDL; a hub or holding
--     is never reclaimed — shares freeze in the hub PDA and holdings are per-wallet
--     ledgers that the program never closes). `share_listing` closes conditionally: a
--     `buy_shares` that drains a listing to zero closes it on the buyer's slot (the
--     program reuses the emptied PDA for the next listing), and `delist_shares` closes
--     it unconditionally (the program closes the account and returns the rent to the
--     seller).
--   * PDA seed notes (for the mapper, which derives pubkeys from instruction accounts):
--     config = "config"; stable = "stable"; hub = "hub" || hub_id (u64); holding =
--     "holding" || hub_id (u64) || holder; listing = "listing" || hub_id (u64) ||
--     seller; share_mint = "share_mint" || next_hub_id (u64, read from the config
--     account, since create_hub takes no hub_id arg); faucet receipt = "faucet" ||
--     caller.
--   * `income_per_share` (hub) and `per_share` (holding) are u128 cumulative counters —
--     they do not fit BIGINT — so they are stored as decimal TEXT (ADR-30), matching
--     how `property`'s u128 fields are handled. `pending` (u64) stays BIGINT.
--   * `next_hub_id`, `bump` (u8 → SMALLINT) and the u32 share counters are stored with
--     the same widened types the other programs use (BIGINT / SMALLINT), never u32.
--   * The holding PDA embeds (hub_id, holder) but neither is stored on the account, and
--     the holding pubkey is not derivable from account data alone, so `realxhub_holding`
--     carries no `holder` column — holder lookups join through `realxhub_hub` + the
--     event tables (SharesBought / IncomeClaimed carry the pubkey). Same for
--     `realxhub_share_listing`: the PDA embeds (hub_id, seller) and the account stores
--     `seller` but not `hub_id`, so only `seller` is denormalized; there is no
--     hub_id column on the listing table.
--
-- All five tables are additive; nothing existing is touched.

-- realxhub Config: { authority: Pubkey, stable_mint: Pubkey, next_hub_id: u64, bump: u8 }
CREATE TABLE realxhub_config (
    pubkey         BYTEA PRIMARY KEY,
    slot           BIGINT NOT NULL,
    lamports       BIGINT NOT NULL,
    closed_at_slot BIGINT,
    authority      BYTEA NOT NULL,
    stable_mint    BYTEA NOT NULL,
    next_hub_id    BIGINT NOT NULL,
    bump           SMALLINT NOT NULL
);

-- realxhub FaucetReceipt: { last_drip: i64, bump: u8 }
CREATE TABLE realxhub_faucet_receipt (
    pubkey         BYTEA PRIMARY KEY,
    slot           BIGINT NOT NULL,
    lamports       BIGINT NOT NULL,
    closed_at_slot BIGINT,
    last_drip      BIGINT NOT NULL,
    bump           SMALLINT NOT NULL
);

-- realxhub Holding: { amount: u32, listed: u32, per_share: u128, pending: u64, bump: u8 }
-- The canonical per-holder ledger for one hub.
CREATE TABLE realxhub_holding (
    pubkey         BYTEA PRIMARY KEY,
    slot           BIGINT NOT NULL,
    lamports       BIGINT NOT NULL,
    closed_at_slot BIGINT,
    amount         BIGINT NOT NULL,
    listed         BIGINT NOT NULL,
    per_share      TEXT NOT NULL,
    pending        BIGINT NOT NULL,
    bump           SMALLINT NOT NULL
);

-- realxhub Hub: { id: u64, name: String, share_mint, operational_spv, supplier,
-- operators, protocol: Pubkey, per_wallet_cap: u32, income_per_share: u128,
-- income_dust: u64, bump: u8 }
CREATE TABLE realxhub_hub (
    pubkey              BYTEA PRIMARY KEY,
    slot                BIGINT NOT NULL,
    lamports            BIGINT NOT NULL,
    closed_at_slot      BIGINT,
    id                  BIGINT NOT NULL,
    name                TEXT NOT NULL,
    share_mint          BYTEA NOT NULL,
    operational_spv     BYTEA NOT NULL,
    supplier            BYTEA NOT NULL,
    operators           BYTEA NOT NULL,
    protocol            BYTEA NOT NULL,
    per_wallet_cap      BIGINT NOT NULL,
    income_per_share    TEXT NOT NULL,
    income_dust         BIGINT NOT NULL,
    bump                SMALLINT NOT NULL
);

-- realxhub ShareListing: { seller: Pubkey, amount: u32, price: u64, bump: u8 }
-- One live listing per (hub, seller).
CREATE TABLE realxhub_share_listing (
    pubkey         BYTEA PRIMARY KEY,
    slot           BIGINT NOT NULL,
    lamports       BIGINT NOT NULL,
    closed_at_slot BIGINT,
    seller         BYTEA NOT NULL,
    amount         BIGINT NOT NULL,
    price          BIGINT NOT NULL,
    bump           SMALLINT NOT NULL
);

CREATE INDEX idx_realxhub_hub_id ON realxhub_hub (id);
CREATE INDEX idx_realxhub_share_listing_seller ON realxhub_share_listing (seller);
