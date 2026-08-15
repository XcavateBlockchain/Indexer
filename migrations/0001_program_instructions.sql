-- Append-only instruction history (spec §5.1, verbatim). Reprocessing is not an edge case
-- here: every stream reconnect replays a tail of recent transactions, and every backfill
-- run overlaps whatever the live stream already wrote. The composite primary key (which
-- transaction, which instruction position, top-level vs CPI) is what makes that safe --
-- callers always write with `ON CONFLICT (signature, ix_index, inner_index) DO NOTHING`, so
-- replaying the same instruction twice is a no-op instead of a duplicate row. Never UPDATE
-- or DELETE a row in this table.
CREATE TABLE program_instructions (
    signature    BYTEA       NOT NULL,
    ix_index     SMALLINT    NOT NULL,
    inner_index  SMALLINT    NOT NULL DEFAULT -1,  -- -1 = top-level, else CPI position
    slot         BIGINT      NOT NULL,
    block_time   TIMESTAMPTZ NOT NULL,
    ix_name      TEXT        NOT NULL,
    accounts     BYTEA[]     NOT NULL,
    data         JSONB       NOT NULL,
    PRIMARY KEY (signature, ix_index, inner_index)
);

CREATE INDEX idx_pi_slot     ON program_instructions (slot);
CREATE INDEX idx_pi_name_time ON program_instructions (ix_name, block_time DESC);
