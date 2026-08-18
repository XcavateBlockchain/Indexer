-- Account-state tables for the `regions` program (FYysH5v23qtz4gK4H1yLDHneFwx6PSAT7oQwHcuRyRh),
-- one per on-chain account type, following 0002's slot-guarded pattern exactly:
--
--   * `pubkey` BYTEA PK, `slot` the write guard (`WHERE t.slot < EXCLUDED.slot`),
--     `closed_at_slot` NULL = live; closes are soft, and any live write at a newer slot
--     revives a closed row (routine for this program -- RegionState and Location PDAs are
--     closed and re-created at the same address across proposal cycles).
--   * Only on-chain fields are stored, so every table stays droppable/rebuildable from a
--     `getProgramAccounts` snapshot.
--   * Integer widths: u8 -> SMALLINT; u16 -> INT (u16 max 65,535 exceeds SMALLINT -- on-chain
--     validation bounds today's values lower, but the column must not rely on it);
--     u32/u64/i64 -> BIGINT (a u64 above i64::MAX wraps to a negative value in the mapper's
--     `as i64` cast -- these are token amounts and counters nowhere near that boundary, the
--     same accepted caveat as `lamports` in 0002).
--   * Enums are TEXT + CHECK over the decoder's variant spellings in SCREAMING_SNAKE_CASE;
--     the borsh variant order behind them is load-bearing (it is what is stored on chain).
--
-- Table names are `regions_<entity>`; the whitelist's tables (0002) keep their legacy
-- unprefixed names.

-- The regions Config PDA (seeds ["config"], singleton).
CREATE TABLE regions_config (
    pubkey                 BYTEA PRIMARY KEY,
    slot                   BIGINT NOT NULL,
    lamports               BIGINT NOT NULL,
    closed_at_slot         BIGINT,
    authority              BYTEA NOT NULL,
    pending_authority      BYTEA,
    xcav_mint              BYTEA NOT NULL,
    minimum_voting_amount  BIGINT NOT NULL,
    voting_period          BIGINT NOT NULL,
    owner_change_period    BIGINT NOT NULL,
    threshold_bps          INT NOT NULL,
    quorum                 BIGINT NOT NULL,
    notice_period          BIGINT NOT NULL,
    min_vote_hold          BIGINT NOT NULL,
    max_listing_duration   BIGINT NOT NULL,
    max_tax_bps            INT NOT NULL,
    location_deposit       BIGINT NOT NULL,
    proposal_counter       BIGINT NOT NULL,
    bump                   SMALLINT NOT NULL
);

-- Location PDAs (seeds ["location", region_id LE, postcode]).
-- `postcode` is on-chain `bytes` (1-10 bytes of uppercase ASCII): BYTEA, not JSONB -- it is a
-- byte string, and `convert_from(postcode, 'UTF8')` renders it. The one variable-length
-- account field in this program.
CREATE TABLE regions_location (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    region_id       INT NOT NULL,
    postcode        BYTEA NOT NULL,
    deposit         BIGINT NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_regions_location_region ON regions_location (region_id);

-- Region PDAs (seeds ["region", region_id LE]).
CREATE TABLE regions_region (
    pubkey               BYTEA PRIMARY KEY,
    slot                 BIGINT NOT NULL,
    lamports             BIGINT NOT NULL,
    closed_at_slot       BIGINT,
    region_id            INT NOT NULL,
    owner                BYTEA NOT NULL,
    collateral           BIGINT NOT NULL,
    location_collateral  BIGINT NOT NULL,
    next_owner_change    BIGINT NOT NULL,
    listing_duration     BIGINT NOT NULL,
    tax_bps              INT NOT NULL,
    location_count       BIGINT NOT NULL,
    bump                 SMALLINT NOT NULL
);
CREATE INDEX idx_regions_region_owner ON regions_region (owner);

-- RegionProposal PDAs (seeds ["proposal", proposal_id LE]; proposal_id is the global
-- monotonic config.proposal_counter, NOT region_id -- many proposals per region over time).
CREATE TABLE regions_region_proposal (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    proposal_id     BIGINT NOT NULL,
    proposer        BYTEA NOT NULL,
    region_id       INT NOT NULL,
    created_at      BIGINT NOT NULL,
    expiry          BIGINT NOT NULL,
    vote_cutoff     BIGINT NOT NULL,
    yes_power       BIGINT NOT NULL,
    no_power        BIGINT NOT NULL,
    abstain_power   BIGINT NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_regions_proposal_region   ON regions_region_proposal (region_id);
CREATE INDEX idx_regions_proposal_id       ON regions_region_proposal (proposal_id);
CREATE INDEX idx_regions_proposal_proposer ON regions_region_proposal (proposer);

-- RegionState PDAs (seeds ["region_state", region_id LE]): the per-region proposal-cycle
-- state machine. Closed by create_region / clear_region_state and re-created by the next
-- propose_new_region for the same region -- the revive-on-newer-slot path is load-bearing.
CREATE TABLE regions_region_state (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    region_id       INT NOT NULL,
    -- Borsh variant order Proposing/Passed/Rejected (0/1/2) is load-bearing.
    status          TEXT NOT NULL CHECK (status IN ('PROPOSING', 'PASSED', 'REJECTED')),
    proposal_id     BIGINT NOT NULL,
    proposer        BYTEA NOT NULL,
    deposit         BIGINT NOT NULL,
    claim_deadline  BIGINT NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_regions_region_state_region ON regions_region_state (region_id);

-- VoteRecord PDAs (seeds ["vote", proposal_id LE, voter]).
CREATE TABLE regions_vote_record (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    proposal_id     BIGINT NOT NULL,
    voter           BYTEA NOT NULL,
    region_id       INT NOT NULL,
    -- Borsh variant order Yes/No/Abstain (0/1/2) is load-bearing.
    vote            TEXT NOT NULL CHECK (vote IN ('YES', 'NO', 'ABSTAIN')),
    power           BIGINT NOT NULL,
    expiry          BIGINT NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_regions_vote_record_proposal ON regions_vote_record (proposal_id);
CREATE INDEX idx_regions_vote_record_voter    ON regions_vote_record (voter);
