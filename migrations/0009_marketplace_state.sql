-- Account-state tables for the `marketplace` program (B6YRVAmjmhN28smZxNfCnuKc19CamBbAEMXsp5KTfWog),
-- following 0002's slot-guarded pattern; see 0008's header for the shared conventions
-- (integer widths, soft closes, revive-on-newer-slot, snake_case `marketplace_` prefix).
--
-- Shape decisions specific to this program:
--   * Fixed-shape nested structs are FLATTENED into typed columns rather than stored as
--     JSONB: `Listing.developer_lawyer` / `Listing.spv_lawyer` (LawyerAssignment -> four
--     `*_lawyer*` columns each, with DocumentStatus as TEXT + CHECK) and
--     `Listing.spv_election` (SpvElection -> three `spv_election_*` columns).
--   * Genuine lists stay JSONB, in shapes this indexer constructs itself (NOT the decoder's
--     serde output): pubkeys as base58 strings, byte-string postcodes as UTF-8 strings, u64
--     amounts as JSON numbers. `Config.accepted_payment_mints` is `["<base58>", ...]` and
--     `Listing.collected` is `[{"mint": "<base58>", "funds": N, "fee": N, "tax": N}, ...]`.
--   * `PropertyAsset.location` is an on-chain `bytes` postcode -> BYTEA (like
--     `regions_location.postcode`).

-- The marketplace Config PDA (seeds ["config"], singleton).
CREATE TABLE marketplace_config (
    pubkey                  BYTEA PRIMARY KEY,
    slot                    BIGINT NOT NULL,
    lamports                BIGINT NOT NULL,
    closed_at_slot          BIGINT,
    authority               BYTEA NOT NULL,
    pending_authority       BYTEA,
    xcav_mint               BYTEA NOT NULL,
    treasury                BYTEA NOT NULL,
    rent_collector          BYTEA NOT NULL,
    accepted_payment_mints  JSONB NOT NULL,
    listing_deposit         BIGINT NOT NULL,
    lawyer_deposit          BIGINT NOT NULL,
    min_property_shares     BIGINT NOT NULL,
    max_property_shares     BIGINT NOT NULL,
    marketplace_fee_bps     INT NOT NULL,
    investor_fee_bps        INT NOT NULL,
    max_ownership_bps       INT NOT NULL,
    claiming_time           BIGINT NOT NULL,
    legal_process_time      BIGINT NOT NULL,
    lawyer_voting_time      BIGINT NOT NULL,
    min_voting_quorum_bps   INT NOT NULL,
    next_listing_id         BIGINT NOT NULL,
    bump                    SMALLINT NOT NULL
);

-- InvestorPosition PDAs (seeds ["position", listing_id LE, investor]). Closed by
-- release_reservation / close_cancelled_position / the three withdraw_* instructions and
-- routinely re-created at the same address by the next buy/reserve.
CREATE TABLE marketplace_investor_position (
    pubkey                 BYTEA PRIMARY KEY,
    slot                   BIGINT NOT NULL,
    lamports               BIGINT NOT NULL,
    closed_at_slot         BIGINT,
    listing_id             BIGINT NOT NULL,
    investor               BYTEA NOT NULL,
    payment_mint           BYTEA NOT NULL,
    payment_account        BYTEA NOT NULL,
    share_amount           BIGINT NOT NULL,
    reserved_share_amount  BIGINT NOT NULL,
    paid_funds             BIGINT NOT NULL,
    paid_tax               BIGINT NOT NULL,
    paid_fee               BIGINT NOT NULL,
    reserved_funds         BIGINT NOT NULL,
    reserved_tax           BIGINT NOT NULL,
    reserved_fee           BIGINT NOT NULL,
    cancelled              BOOLEAN NOT NULL,
    bump                   SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_position_listing  ON marketplace_investor_position (listing_id);
CREATE INDEX idx_mkt_position_investor ON marketplace_investor_position (investor);

-- Lawyer registry PDAs (seeds ["lawyer", lawyer]).
CREATE TABLE marketplace_lawyer (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    lawyer          BYTEA NOT NULL,
    region_id       INT NOT NULL,
    deposit         BIGINT NOT NULL,
    active_cases    BIGINT NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_lawyer_wallet ON marketplace_lawyer (lawyer);

-- LawyerCandidacy PDAs (seeds ["lawyer-candidate", listing_id LE, round LE, lawyer]).
CREATE TABLE marketplace_lawyer_candidacy (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    listing_id      BIGINT NOT NULL,
    round           BIGINT NOT NULL,
    lawyer          BYTEA NOT NULL,
    costs           BIGINT NOT NULL,
    vote_power      BIGINT NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_candidacy_listing ON marketplace_lawyer_candidacy (listing_id, round);

-- LawyerVote PDAs (seeds ["lawyer-vote", listing_id LE, round LE, voter]).
CREATE TABLE marketplace_lawyer_vote (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    listing_id      BIGINT NOT NULL,
    round           BIGINT NOT NULL,
    voter           BYTEA NOT NULL,
    choice          BYTEA NOT NULL,
    power           BIGINT NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_lawyer_vote_listing ON marketplace_lawyer_vote (listing_id, round);
CREATE INDEX idx_mkt_lawyer_vote_voter   ON marketplace_lawyer_vote (voter);

-- Listing PDAs (seeds ["listing", listing_id LE]; listing_id allocated from
-- config.next_listing_id). The LawyerAssignment structs and SpvElection are flattened; the
-- DocumentStatus borsh order Pending/Approved/Rejected (0/1/2) is load-bearing, as is
-- ListingStatus's PendingAssets..Refunding (0..7).
CREATE TABLE marketplace_listing (
    pubkey                          BYTEA PRIMARY KEY,
    slot                            BIGINT NOT NULL,
    lamports                        BIGINT NOT NULL,
    closed_at_slot                  BIGINT,
    listing_id                      BIGINT NOT NULL,
    developer                       BYTEA NOT NULL,
    asset_id                        BIGINT NOT NULL,
    share_price                     BIGINT NOT NULL,
    listed_share_amount             BIGINT NOT NULL,
    sold_share_amount               BIGINT NOT NULL,
    reserved_share_amount           BIGINT NOT NULL,
    tax_paid_by_developer           BOOLEAN NOT NULL,
    tax_bps                         INT NOT NULL,
    marketplace_fee_bps             INT NOT NULL,
    investor_fee_bps                INT NOT NULL,
    max_ownership_bps               INT NOT NULL,
    listing_expiry                  BIGINT NOT NULL,
    claiming_time                   BIGINT NOT NULL,
    claim_deadline                  BIGINT NOT NULL,
    legal_process_time              BIGINT NOT NULL,
    lawyer_voting_time              BIGINT NOT NULL,
    min_voting_quorum_bps           INT NOT NULL,
    position_count                  BIGINT NOT NULL,
    legal_deadline                  BIGINT NOT NULL,
    deposit                         BIGINT NOT NULL,
    -- Listing.developer_lawyer (LawyerAssignment), flattened.
    developer_lawyer                BYTEA NOT NULL,
    developer_lawyer_costs          BIGINT NOT NULL,
    developer_lawyer_doc_status     TEXT NOT NULL
        CHECK (developer_lawyer_doc_status IN ('PENDING', 'APPROVED', 'REJECTED')),
    developer_lawyer_documents_hash BYTEA NOT NULL,
    -- Listing.spv_lawyer (LawyerAssignment), flattened.
    spv_lawyer                      BYTEA NOT NULL,
    spv_lawyer_costs                BIGINT NOT NULL,
    spv_lawyer_doc_status           TEXT NOT NULL
        CHECK (spv_lawyer_doc_status IN ('PENDING', 'APPROVED', 'REJECTED')),
    spv_lawyer_documents_hash       BYTEA NOT NULL,
    second_attempt                  BOOLEAN NOT NULL,
    developer_engaged               BOOLEAN NOT NULL,
    spv_costs_due                   BIGINT NOT NULL,
    spv_costs_payee                 BYTEA NOT NULL,
    collected                       JSONB NOT NULL,
    -- Listing.spv_election (SpvElection), flattened.
    spv_election_expiry             BIGINT NOT NULL,
    spv_election_candidate_count    BIGINT NOT NULL,
    spv_election_round              BIGINT NOT NULL,
    status                          TEXT NOT NULL CHECK (status IN
        ('PENDING_ASSETS', 'LISTED', 'SOLD_OUT', 'LEGAL', 'FINALIZED', 'EXPIRED',
         'CANCELLED', 'REFUNDING')),
    bump                            SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_listing_id        ON marketplace_listing (listing_id);
CREATE INDEX idx_mkt_listing_developer ON marketplace_listing (developer);
CREATE INDEX idx_mkt_listing_status    ON marketplace_listing (status);

-- PropertyAsset PDAs (seeds ["property", listing_id LE]; asset_id == listing_id in current
-- source). `core_asset` is only ever the all-zero pubkey today; `location` is the raw
-- postcode byte string.
CREATE TABLE marketplace_property_asset (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    core_asset      BYTEA NOT NULL,
    share_mint      BYTEA NOT NULL,
    region_id       INT NOT NULL,
    location        BYTEA NOT NULL,
    share_amount    BIGINT NOT NULL,
    spv_created     BOOLEAN NOT NULL,
    finalized       BOOLEAN NOT NULL,
    holder_count    BIGINT NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_property_asset_id ON marketplace_property_asset (asset_id);

-- Reservation PDAs (seeds ["reservation", token_account] -- keyed by the investor's payment
-- token account, shared across listings). Closed at zero and re-created by the next reserve.
CREATE TABLE marketplace_reservation (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    token_account   BYTEA NOT NULL,
    amount          BIGINT NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_reservation_token_account ON marketplace_reservation (token_account);

-- ShareHolding PDAs (seeds ["share", asset_id LE, owner]).
CREATE TABLE marketplace_share_holding (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    owner           BYTEA NOT NULL,
    amount          BIGINT NOT NULL,
    locked_amount   BIGINT NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_mkt_share_holding_asset ON marketplace_share_holding (asset_id);
CREATE INDEX idx_mkt_share_holding_owner ON marketplace_share_holding (owner);
