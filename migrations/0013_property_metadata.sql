-- Off-chain property metadata, decomposed (ADR-27).
--
-- The marketplace `PropertyAsset` account's `metadata_uri` (added in 0012) points at a
-- JSON document hosted by the protocol team: the property's human-readable details
-- (description, address, attributes, finances, document/image links) that the Core-asset
-- NFT deed used to carry. This table is where the indexer's background fetcher
-- (`crates/indexer/src/metadata.rs`) stores the fetched-and-decomposed document.
--
-- SHAPE -- a derived table, NOT an account-state table. Everything else in this schema is
-- a slot-guarded mirror of an on-chain account (0002's contract: rebuildable from a
-- `getProgramAccounts` snapshot, soft-closable, slot-guarded). This one is not:
--   * no `slot` / `lamports` / `closed_at_slot` -- there is no on-chain account to mirror
--     or close; the row's lifetime is owned by the fetcher's upserts;
--   * NOT in `db::close::StateTable`, so no close, no snapshot close-missing sweep, and no
--     entry in `programs.rs`' rosters (the roster test enforces the partition stays clean);
--   * keyed by the PropertyAsset PDA's `pubkey` (1:1 with `marketplace_property_asset`),
--     with `asset_id` carried for joins.
-- A devnet reset / volume drop wipes it with everything else; `indexer fetch-metadata`
-- (or the live fetcher loop) refills it.
--
-- COLUMN SHAPES:
--   * The document's nested `address` / `attributes` / `finances` objects are FLATTENED
--     into typed columns (0010's convention for fixed nested structs; `address.*` takes an
--     `address_` prefix, the others are flat because their fields are unambiguous).
--   * `propertyImages` / `otherDocuments` are genuine string lists -> JSONB arrays of URL
--     strings, in the shape the INDEXER constructs itself (0009/0010's JSONB rule: never
--     the serde output of some crate).
--   * `user_pubkey` / `companyWalletAddress` are base58 Solana wallet addresses -> BYTEA (the
--     pubkey convention, spelled `user_pubkey` after the whitelist tables because `user` is
--     a Postgres reserved word), decoded by the fetcher; an invalid base58 string stores
--     NULL in that column and nothing else is affected (lenient per-field parsing, ADR-27).
--   * `raw` holds the WHOLE fetched document verbatim: the ground truth the typed columns
--     are derived from, so a future document field costs no migration and a mis-typed
--     field is always auditable.
--   * All content columns are NULLABLE: a document missing a key (or with a mis-typed
--     value) stores NULL there -- the fetcher's parsing is lenient per field, so a partial
--     document still indexes its good fields.
--
-- FETCH STATE (the loop's retry machinery, read by the work-set query in
-- `db::property_metadata`):
--   * `metadata_uri` = the URI the LAST ATTEMPT targeted (success or failure);
--   * `fetched_at` / `raw` / the content columns = the LAST SUCCESSFUL snapshot (NULL
--     until one exists);
--   * `attempts` = consecutive failures for `metadata_uri` (reset to 0 by a success);
--   * `next_attempt_at` = backoff deadline (30 s doubling per failure, 1 h cap, computed
--     in the failure upsert); NULL after a success;
--   * `last_error` = the last failure's message; NULL after a success.

CREATE TABLE marketplace_property_metadata (
    pubkey                  BYTEA PRIMARY KEY,
    asset_id                BIGINT NOT NULL,
    metadata_uri            TEXT NOT NULL,
    -- Fetch state.
    fetched_at             TIMESTAMPTZ,
    attempts               INT NOT NULL DEFAULT 0,
    next_attempt_at        TIMESTAMPTZ,
    last_error             TEXT,
    -- The whole document, verbatim (the typed columns below are derived from it).
    raw                    JSONB,
    -- Identity / description (top-level document fields).
    property_id            TEXT,
    property_name          TEXT,
    property_type          TEXT,
    status                 TEXT,
    tenure                 TEXT,
    property_description   TEXT,
    planning_code          TEXT,
    building_control_code  TEXT,
    user_pubkey            BYTEA,
    company_id             TEXT,
    company_name           TEXT,
    company_logo           TEXT,
    company_wallet_address BYTEA,
    created_at             TIMESTAMPTZ,
    updated_at             TIMESTAMPTZ,
    -- `address` object, flattened.
    address_street         TEXT,
    address_town_city      TEXT,
    address_flat_or_unit   TEXT,
    address_post_code      TEXT,
    address_local_authority TEXT,
    address_region         TEXT,
    address_location       TEXT,
    -- `attributes` object, flattened.
    area                   TEXT,
    quality                TEXT,
    outdoor_space          TEXT,
    number_of_bedrooms     BIGINT,
    number_of_bathrooms    BIGINT,
    construction_date      DATE,
    off_street_parking     TEXT,
    -- `finances` object, flattened.
    property_price         BIGINT,
    number_of_shares       BIGINT,
    share_price            BIGINT,
    estimated_rental_income BIGINT,
    annual_service_charge  BIGINT,
    stamp_duty_tax         BIGINT,
    stamp_duty_paid        BOOLEAN,
    annual_service_charge_paid BOOLEAN,
    -- Documents / media (top-level).
    floor_plan             TEXT,
    map_url                TEXT,
    sales_agreement        TEXT,
    other_documents        JSONB,
    property_images        JSONB
);
CREATE INDEX idx_mkt_property_metadata_asset ON marketplace_property_metadata (asset_id);
