-- Account state: current only, one row per PDA (spec §5.2). Every write to these tables
-- (see crates/indexer/src/db/accounts.rs) goes through
--     INSERT ... ON CONFLICT (pubkey) DO UPDATE SET ... WHERE <table>.slot < EXCLUDED.slot
-- `slot` here is a write guard, not history: it is the slot of the account update that
-- produced the currently-stored row, and the WHERE clause is what makes the guard work.
-- Without it, a `getProgramAccounts` snapshot load racing a live stream reconnect can
-- silently overwrite fresh state with a stale snapshot -- there is no error, no constraint
-- violation, just a row that quietly goes backwards. A close is the same kind of
-- slot-guarded write (it sets closed_at_slot and bumps slot; it never DELETEs), and a later
-- re-CREATE of the same PDA at a higher slot clears closed_at_slot back to NULL because the
-- normal account write always includes `closed_at_slot = NULL` in its column list. This is
-- also why these tables carry only fields that exist on-chain: they must stay droppable and
-- rebuildable from a fresh snapshot at any time.

CREATE TABLE config (
    pubkey             BYTEA PRIMARY KEY,
    slot               BIGINT NOT NULL,
    lamports           BIGINT NOT NULL,
    closed_at_slot     BIGINT,              -- NULL = live
    authority          BYTEA NOT NULL,
    pending_authority  BYTEA,               -- on-chain Option<pubkey>
    bump               SMALLINT NOT NULL
);

CREATE TABLE admin (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,                 -- NULL = live
    admin           BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);

-- `role` and `permission` are stored as TEXT + CHECK (there is no native Postgres enum type
-- involved) using the OLD schema's spellings, so downstream consumers of role_assignments_view
-- and the old grpc-api queries don't have to change. The borsh variant index behind each
-- spelling is load-bearing on-chain (RoleAccount.role/permission store the index, and the
-- PDA seed for RoleAccount embeds the role byte) -- do not reorder, insert, or remove
-- variants below without a data migration:
--   Role:             0 REGIONAL_OPERATOR, 1 REAL_ESTATE_INVESTOR, 2 REAL_ESTATE_DEVELOPER,
--                     3 LAWYER, 4 LETTING_AGENT, 5 SPV_CONFIRMATION
--   AccessPermission: 0 COMPLIANT, 1 REVOKED
CREATE TABLE role_account (
    pubkey          BYTEA PRIMARY KEY,
    slot            BIGINT NOT NULL,
    lamports        BIGINT NOT NULL,
    closed_at_slot  BIGINT,                 -- NULL = live
    -- Mirrors the on-chain `user` field; renamed to dodge the SQL reserved word.
    user_pubkey     BYTEA NOT NULL,
    role            TEXT NOT NULL CHECK (role IN (
        'REGIONAL_OPERATOR', 'REAL_ESTATE_INVESTOR', 'REAL_ESTATE_DEVELOPER', 'LAWYER',
        'LETTING_AGENT', 'SPV_CONFIRMATION')),
    permission      TEXT NOT NULL CHECK (permission IN ('COMPLIANT', 'REVOKED')),
    rent_payer      BYTEA NOT NULL,
    bump            SMALLINT NOT NULL
);
