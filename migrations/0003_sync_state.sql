-- Sync state (spec §5.3, verbatim). Pipeline bookkeeping, not on-chain data: one singleton
-- row (enforced by `id = 1` + the PK). `last_contiguous_slot` is the highest slot below
-- which there are no gaps -- NOT simply the highest slot seen -- so it must only move
-- forward once backfill/live-stream progress has actually closed every hole up to it;
-- reporting the naive "highest slot seen" would read healthy while gaps remain underneath.
CREATE TABLE sync_state (
    id                      SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    last_contiguous_slot    BIGINT NOT NULL,
    backfill_complete       BOOLEAN NOT NULL DEFAULT FALSE,
    backfill_floor_slot     BIGINT NOT NULL,
    snapshot_slot           BIGINT,
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);
