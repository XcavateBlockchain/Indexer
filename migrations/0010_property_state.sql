-- Account-state tables for the `property` program (8f4NHc1wGBM1BAufDFd9dNechLW8pxmStSfxfuJfDzob),
-- following 0002's slot-guarded pattern; see 0008's header for the shared conventions
-- (integer widths, soft closes, revive-on-newer-slot, snake_case `property_` prefix).
--
-- One naming exception: the `PropertyLetting` account type's table is `property_letting`,
-- not `property_property_letting` -- when the entity name already starts with the program
-- name, the duplicated word collapses.
--
-- This program's account fields contain no enums. Two structured fields deviate from
-- plain typed columns:
--   * `LettingAgent.locations` (Vec of {postcode, assigned_count, deposit}, max 10 entries)
--     -> JSONB, in a shape the indexer constructs itself (NOT the decoder's serde output):
--     `[{"postcode": "E14", "assigned_count": 0, "deposit": 1000000}, ...]` -- postcodes as
--     UTF-8 strings (on-chain validated ASCII). The conditional letting-agent close compares
--     against this exact shape.
--   * `PropertyLetting.election` (a fixed 4-field nested struct) -> flattened to
--     `election_*` columns; its fields (round, expiry) are query-relevant, and flattening
--     keeps the typed-column contract.

-- The property Config PDA (seeds ["config"], singleton).
CREATE TABLE property_config (
    pubkey                 BYTEA PRIMARY KEY,
    slot                   BIGINT NOT NULL,
    lamports               BIGINT NOT NULL,
    closed_at_slot         BIGINT,
    authority              BYTEA NOT NULL,
    pending_authority      BYTEA,
    xcav_mint              BYTEA NOT NULL,
    treasury               BYTEA NOT NULL,
    rent_collector         BYTEA NOT NULL,
    agent_deposit          BIGINT NOT NULL,
    agent_voting_time      BIGINT NOT NULL,
    min_voting_quorum_bps  INT NOT NULL,
    agent_notice_period    BIGINT NOT NULL,
    bump                   SMALLINT NOT NULL
);

-- AgentCandidacy PDAs (seeds ["agent-candidate", asset_id LE, round LE, agent]).
CREATE TABLE property_agent_candidacy (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    round           BIGINT NOT NULL,
    agent           BYTEA NOT NULL,
    vote_power      BIGINT NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_property_candidacy_asset ON property_agent_candidacy (asset_id, round);

-- AgentVote PDAs (seeds ["agent-vote", asset_id LE, round LE, voter]).
CREATE TABLE property_agent_vote (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    round           BIGINT NOT NULL,
    voter           BYTEA NOT NULL,
    choice          BYTEA NOT NULL,
    power           BIGINT NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_property_agent_vote_asset ON property_agent_vote (asset_id, round);
CREATE INDEX idx_property_agent_vote_voter ON property_agent_vote (voter);

-- LettingAgent PDAs (seeds ["agent", wallet]). `locations` is the one JSONB field (see the
-- header); an agent whose last location is removed has this PDA closed on-chain by a runtime
-- `close()` call (NOT an Anchor `close =` constraint) -- see the batcher's conditional-close
-- op for how that reaches `closed_at_slot`.
CREATE TABLE property_letting_agent (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    wallet          BYTEA NOT NULL,
    region_id       INT NOT NULL,
    locations       JSONB NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_property_letting_agent_wallet ON property_letting_agent (wallet);

-- PropertyLetting PDAs (seeds ["letting", asset_id LE]): one per listed property, holding the
-- letting-agent seat and the current election. `agent` uses the all-zero pubkey as the
-- on-chain "seat vacant" sentinel -- stored verbatim, NOT translated to NULL, so vacancy
-- queries match on-chain semantics.
CREATE TABLE property_letting (
    pubkey                   BYTEA PRIMARY KEY,
    slot                     BIGINT NOT NULL,
    lamports                 BIGINT NOT NULL,
    closed_at_slot           BIGINT,
    asset_id                 BIGINT NOT NULL,
    agent                    BYTEA NOT NULL,
    -- The AgentElection nested struct, flattened.
    election_expiry          BIGINT NOT NULL,
    election_candidate_count BIGINT NOT NULL,
    election_round           BIGINT NOT NULL,
    election_quorum_bps      INT NOT NULL,
    rent_payer               BYTEA NOT NULL,
    bump                     SMALLINT NOT NULL
);
CREATE INDEX idx_property_letting_asset ON property_letting (asset_id);
CREATE INDEX idx_property_letting_agent ON property_letting (agent);

-- ResignationNotice PDAs (seeds ["resignation", asset_id LE]) -- one per property, not per
-- agent.
CREATE TABLE property_resignation_notice (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,
    asset_id        BIGINT NOT NULL,
    agent           BYTEA NOT NULL,
    due_ts          BIGINT NOT NULL,
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
CREATE INDEX idx_property_resignation_asset ON property_resignation_notice (asset_id);
