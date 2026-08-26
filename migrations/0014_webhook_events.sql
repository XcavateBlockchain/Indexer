-- Outbound webhook events, recorded durably and delivered by a background loop (ADR-28).
--
-- The first (and for now only) event type is `property_asset_registered`: the marketplace
-- program's `init_property_assets` instruction (the moment a `PropertyAsset` gets its name +
-- metadata_uri + share mint -- the registration the ADR-27 metadata fetcher already keys on).
-- The marketplace mapper emits one [`crate::batcher::WriteOp::RecordWebhookEvent`] per such
-- instruction; the batcher commits it here with `ON CONFLICT (event_id) DO NOTHING`.
--
-- WHY A TABLE AND NOT A FIRE-AND-FORGET POST: the pipeline has two writers that re-derive
-- history independently (the live stream and the backfill/reconciliation crawls, ADR-15), so a
-- notification fired straight from the write path would fire once per re-walk. The durable row
-- is the "this asset was registered" record that is idempotent under re-walks (a backfill
-- re-delivering a historical `init_property_assets` is an `ON CONFLICT` no-op), and it is also
-- the delivery queue: the background loop (`crates/indexer/src/webhooks.rs`) reads the
-- undelivered rows, POSTs each `payload` to `WEBHOOK_URL`, and marks it delivered on a 2xx.
--
-- SHAPE -- an event table, NOT an account-state table (same exclusion as
-- `marketplace_property_metadata`, ADR-27): no `slot` guard, no soft close, no
-- `db::close::StateTable` entry, no `ProgramSpec.tables` roster. The `slot` / `block_time` /
-- `tx_signature` columns are the on-chain COORDINATES of the registration (provenance), not a
-- mirror slot guard. A devnet reset / volume drop wipes it with everything else; the crawls
-- re-derive the rows (and the loop re-delivers any that were never delivered).
--
-- EVENT IDENTITY: `event_id` is `<event_type>:<base58 subject key>` -- for
-- `property_asset_registered`, the subject is the `PropertyAsset` PDA (seeded
-- `["property", listing_id]`, one per listing). It is the `ON CONFLICT` key, so each asset is
-- announced at most once, ever. The loop's per-row delivery state:
--   * `attempts`        = consecutive failed deliveries (reset to 0 by a success);
--   * `next_attempt_at` = backoff deadline (30 s, doubling per failure, 1 h cap, computed in the
--                         failure update); NULL after a success;
--   * `last_error`      = the last failure's message; NULL after a success;
--   * `delivered_at`    = when the loop got a 2xx (NULL = still pending / being retried).

CREATE TABLE webhook_events (
    event_id        TEXT PRIMARY KEY,
    event_type      TEXT NOT NULL,
    -- The JSON document the delivery loop POSTs to WEBHOOK_URL, verbatim.
    payload         JSONB NOT NULL,
    -- On-chain coordinates of the registration (provenance), not a mirror slot guard.
    slot            BIGINT NOT NULL,
    tx_signature    TEXT NOT NULL,
    block_time      TIMESTAMPTZ NOT NULL,
    -- When this row was first recorded (detection time), independent of delivery.
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Delivery state (the loop's retry machinery; see the header).
    attempts        INT NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ,
    last_error      TEXT,
    delivered_at    TIMESTAMPTZ
);

-- The delivery loop's work-set query scans the undelivered rows ordered by event_id; the
-- partial index keeps that scan cheap as the delivered backlog grows (a delivered row leaves
-- the pending set for good).
CREATE INDEX webhook_events_pending_idx ON webhook_events (event_id) WHERE delivered_at IS NULL;
