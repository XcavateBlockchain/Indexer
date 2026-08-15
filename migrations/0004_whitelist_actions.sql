-- Append-only parity table (spec §5.4a, controller ruling R7). Reproduces the old SubQuery
-- WhitelistAction entity as a plain event log with NO order-sensitive incremental state --
-- unlike the old TypeScript handlers, which mutated Admin/RoleAssignment rows in place as
-- events arrived and therefore depended on seeing events in chain order. Writing every event
-- here instead, and folding derived state out of it in SQL views (see
-- migrations/0005_derived_views.sql), is what makes live-stream + backwards-backfill overlap
-- safe: it no longer matters which of the two writes a given row first. Written by the
-- instruction processor (Task 3) in the same DB transaction as the corresponding
-- program_instructions row; both writes are idempotent (`ON CONFLICT (id) DO NOTHING`), so
-- replaying a transaction is a no-op here too.
CREATE TABLE whitelist_actions (
    id                TEXT PRIMARY KEY,          -- "<tx_signature>-<ix_path>" (old identity)
    type              TEXT NOT NULL CHECK (type IN (
        'CONFIG_INITIALIZED','AUTHORITY_UPDATE_PROPOSED','AUTHORITY_UPDATED',
        'ADMIN_ADDED','ADMIN_REMOVED','ROLE_ASSIGNED','ROLE_REMOVED',
        'ROLE_RENOUNCED','PERMISSION_UPDATED')),
    subject           TEXT,                      -- base58; affected address
    role              TEXT,                      -- old Role spellings, NULL when n/a
    permission        TEXT,                      -- COMPLIANT/REVOKED, NULL when n/a
    actor             TEXT NOT NULL,             -- base58 signer
    slot              BIGINT NOT NULL,           -- NOTE: slots, not block heights (ruling R8)
    block_time        TIMESTAMPTZ NOT NULL,
    tx_signature      TEXT NOT NULL,             -- base58
    instruction_index TEXT NOT NULL              -- dot-joined, e.g. "3" or "3.1"
);
CREATE INDEX idx_wa_subject ON whitelist_actions (subject);
CREATE INDEX idx_wa_type    ON whitelist_actions (type);
CREATE INDEX idx_wa_actor   ON whitelist_actions (actor);
CREATE INDEX idx_wa_txsig   ON whitelist_actions (tx_signature);
