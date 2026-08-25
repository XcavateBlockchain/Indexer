-- Schema for the 2026-08-25 protocol redeploy: all four programs were REDEPLOYED at new
-- addresses (addresses.json; the old deployments are abandoned, not upgraded -- ADR-26).
-- The marketplace gained a secondary share market (ShareListing + Offer) and per-reason
-- share locks; the property program gained holder governance (Proposal / Challenge /
-- GovVote) and rental income (PropertyIncome / IncomeCheckpoint). This migration follows
-- 0008's shared conventions (integer widths, soft closes, revive-on-newer-slot, snake_case
-- program prefixes; the 0010 naming rule collapses `property_property_*` to `property_*`).
--
-- Destructive statements, and why they are correct: the redeploy orphans EVERY row the
-- indexer ever wrote -- old-address accounts no longer exist on-chain and the old programs
-- will never execute again -- so production is rebuilt from an empty database (RUNBOOK
-- "Devnet ledger reset", docs/deployment.md section 5), exactly as after a devnet ledger
-- reset. The two DROP COLUMNs below therefore reshape tables whose every surviving row is
-- already dead data awaiting that wipe; no live row can lose information. On a fresh
-- database this migration runs against empty 0009/0010 tables and the DROPs are pure
-- schema. New NOT NULL columns carry DEFAULTs so the ALTERs are also valid on a not yet
-- wiped database; the upserts always write every column explicitly, so the defaults are
-- never load-bearing.
-- lint: allow DROP COLUMN -- every pre-0012 row describes an account of the abandoned pre-redeploy deployments and the database is rebuilt from empty (see header); nothing live is destroyed

-- --- marketplace: changed account shapes --------------------------------------------------

-- Config gained the secondary-market id allocator.
ALTER TABLE marketplace_config
    ADD COLUMN next_share_listing_id BIGINT NOT NULL DEFAULT 0;

-- Listing gained the fee-quote accumulator that caps the SPV lawyer's charge.
ALTER TABLE marketplace_listing
    ADD COLUMN collected_fee_quote BIGINT NOT NULL DEFAULT 0;

-- PropertyAsset: the Core-asset NFT deed was dropped upstream ("drop property nfts") in
-- favour of on-account name + metadata URI.
ALTER TABLE marketplace_property_asset DROP COLUMN core_asset;
ALTER TABLE marketplace_property_asset
    ADD COLUMN name         TEXT NOT NULL DEFAULT '',
    ADD COLUMN metadata_uri TEXT NOT NULL DEFAULT '';

-- ShareHolding: the single `locked_amount` became one counter per on-chain `LockReason`
-- variant (a fixed-shape [u32; 4] -> flattened to typed columns, the 0009 convention; the
-- effective lock is GREATEST of the four), plus `listed` (shares committed to open
-- secondary listings, still counted in `amount`).
ALTER TABLE marketplace_share_holding DROP COLUMN locked_amount;
ALTER TABLE marketplace_share_holding
    ADD COLUMN lock_lawyer_election BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN lock_agent_election  BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN lock_proposal        BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN lock_challenge       BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN listed               BIGINT NOT NULL DEFAULT 0;

-- --- marketplace: the secondary share market ----------------------------------------------

-- ShareListing PDAs (seeds ["share-listing", id LE]; id allocated from
-- config.next_share_listing_id). Closed by delist_shares and, when the last share sells, by
-- accept_offer / buy_relisted_shares (runtime closes -- see the mapping's close table).
CREATE TABLE marketplace_share_listing (
    pubkey            BYTEA PRIMARY KEY,
    slot              BIGINT NOT NULL,
    lamports          BIGINT NOT NULL,
    closed_at_slot    BIGINT,
    id                BIGINT NOT NULL,
    asset_id          BIGINT NOT NULL,
    seller            BYTEA NOT NULL,
    share_price       BIGINT NOT NULL,
    amount            BIGINT NOT NULL,
    fee_bps           INT NOT NULL,
    next_offer_nonce  BIGINT NOT NULL,
    rent_payer        BYTEA NOT NULL,
    bump              SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_share_listing_id     ON marketplace_share_listing (id);
CREATE INDEX idx_mkt_share_listing_asset  ON marketplace_share_listing (asset_id);
CREATE INDEX idx_mkt_share_listing_seller ON marketplace_share_listing (seller);

-- Offer PDAs (seeds ["offer", listing_id LE, offeror] -- one open offer per bidder per share
-- listing; `listing_id` is the ShareListing id, NOT a primary listing_id). The bid money
-- sits in the offer's own vault token account, which is not a state table. Closed by
-- accept_offer / reject_offer / cancel_offer.
CREATE TABLE marketplace_offer (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    listing_id      BIGINT NOT NULL,
    asset_id        BIGINT NOT NULL,
    offeror         BYTEA NOT NULL,
    share_price     BIGINT NOT NULL,
    amount          BIGINT NOT NULL,
    payment_mint    BYTEA NOT NULL,
    held            BIGINT NOT NULL,
    nonce           BIGINT NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_offer_listing ON marketplace_offer (listing_id);
CREATE INDEX idx_mkt_offer_offeror ON marketplace_offer (offeror);

-- --- property: changed account shapes -----------------------------------------------------

-- Config gained the governance parameters (proposals, challenges, income never touch it).
ALTER TABLE property_config
    ADD COLUMN proposal_voting_time  BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN low_proposal          BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN high_proposal         BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN high_threshold_bps    INT NOT NULL DEFAULT 0,
    ADD COLUMN auto_approval_cooldown BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN challenge_deposit     BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN agent_slash_amount    BIGINT NOT NULL DEFAULT 0;

-- PropertyLetting gained the GovState nested struct (one live proposal + one live challenge
-- per property, strikes against the sitting agent) -> flattened to `governance_*` columns,
-- like `election_*` in 0010.
ALTER TABLE property_letting
    ADD COLUMN governance_proposal_count        BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN governance_challenge_count       BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN governance_active_proposal       BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN governance_active_challenge      BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN governance_strikes               SMALLINT NOT NULL DEFAULT 0,
    ADD COLUMN governance_last_auto_approval_ts BIGINT NOT NULL DEFAULT 0;

-- --- property: holder governance ----------------------------------------------------------

-- Proposal PDAs (seeds ["proposal", asset_id LE, id LE]). A proposal at or under the
-- config's low tier closes inside its own `propose` instruction (auto-approval) and never
-- reaches this table; the rest are closed by finalize_proposal (a runtime close). The
-- final tally and outcome die with the account -- see the ADR-10 event-audit note in the
-- redeploy's MIGRATION_LOG entry.
CREATE TABLE property_proposal (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    id              BIGINT NOT NULL,
    proposer        BYTEA NOT NULL,
    amount          BIGINT NOT NULL,
    details_hash    BYTEA NOT NULL,
    expiry          BIGINT NOT NULL,
    -- The Tally nested struct, flattened.
    tally_yes       BIGINT NOT NULL,
    tally_no        BIGINT NOT NULL,
    tally_abstain   BIGINT NOT NULL,
    quorum_bps      INT NOT NULL,
    threshold_bps   INT NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_property_proposal_asset ON property_proposal (asset_id, id);

-- Challenge PDAs (seeds ["challenge", asset_id LE, id LE]). Closed by finalize_challenge (a
-- runtime close); the outcome (passed/slashed/removed) dies with the account, same ADR-10
-- note as property_proposal.
CREATE TABLE property_challenge (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    id              BIGINT NOT NULL,
    challenger      BYTEA NOT NULL,
    agent           BYTEA NOT NULL,
    deposit         BIGINT NOT NULL,
    expiry          BIGINT NOT NULL,
    -- The Tally nested struct, flattened.
    tally_yes       BIGINT NOT NULL,
    tally_no        BIGINT NOT NULL,
    tally_abstain   BIGINT NOT NULL,
    quorum_bps      INT NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_property_challenge_asset ON property_challenge (asset_id, id);

-- GovVote PDAs: one account type behind TWO seed families -- proposal votes
-- (["proposal-vote", asset_id LE, id LE, voter]) and challenge votes (["challenge-vote",
-- ...]). The account data carries no discriminator between the two (only the seed prefix
-- differs), so rows deliberately have no kind column: joining `id` against
-- property_proposal / property_challenge is the consumer's disambiguation. Closed by
-- unlock_proposal_votes / unlock_challenge_votes. The borsh order of VoteChoice
-- (Yes/No/Abstain = 0/1/2) is load-bearing, like every TEXT enum here.
CREATE TABLE property_gov_vote (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    id              BIGINT NOT NULL,
    voter           BYTEA NOT NULL,
    choice          TEXT NOT NULL CHECK (choice IN ('YES', 'NO', 'ABSTAIN')),
    power           BIGINT NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_property_gov_vote_asset ON property_gov_vote (asset_id, id);
CREATE INDEX idx_property_gov_vote_voter ON property_gov_vote (voter);

-- --- property: rental income --------------------------------------------------------------

-- PropertyIncome PDAs (seeds ["income", asset_id LE]): one per property, created by the
-- first distribution, never closed. `streams` is a genuine list -> JSONB, in a shape this
-- indexer constructs itself: `[{"mint": "<base58>", "per_share": "<decimal string>",
-- "dust": N}, ...]`. `per_share` is u128 on-chain and is stored as a DECIMAL STRING --
-- serde_json's number type cannot carry the full range, and a string survives round-trips
-- exactly.
CREATE TABLE property_income (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    streams         JSONB NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_property_income_asset ON property_income (asset_id);

-- IncomeCheckpoint PDAs (seeds ["checkpoint", asset_id LE, owner]): one holder's claim
-- state, entries[i] tracking streams[i]. Same JSONB conventions as property_income:
-- `[{"per_share": "<decimal string>", "pending": N}, ...]`. Closed by
-- close_income_checkpoint and routinely re-created at the same address by the next claim
-- or settle.
CREATE TABLE property_income_checkpoint (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    owner           BYTEA NOT NULL,
    entries         JSONB NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_property_income_checkpoint_asset ON property_income_checkpoint (asset_id);
CREATE INDEX idx_property_income_checkpoint_owner ON property_income_checkpoint (owner);
