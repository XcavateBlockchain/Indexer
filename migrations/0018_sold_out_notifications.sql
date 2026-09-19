-- Sold-out claim notifications, recorded durably and delivered by a background loop (ADR-35).
--
-- When a marketplace `Listing` flips to `SoldOut` on-chain, every investor still holding a
-- reservation on that listing must be told to claim: the delivery loop POSTs one push
-- notification per reserver wallet to the Xcavate notifications API
-- (`POST {NOTIFICATIONS_API_URL}/api/fcm/send-notification/`, `Authorization: Api-Key ...`).
-- This table is both the durable "this listing sold out" record and the delivery queue.
--
-- WHY A TABLE AND NOT A FIRE-AND-FORGET POST: identical argument to 0014 (ADR-28) -- the
-- pipeline has two writers that re-derive history independently (the live stream and the
-- backfill/reconciliation crawls, ADR-15), so a notification fired straight from the write
-- path would fire once per re-walk, and a POST on the write path would couple ingestion of
-- all five programs to a third-party endpoint. Detection is an ACCOUNT-STATE transition, not
-- an instruction (ADR-28's precedent keys off `init_property_assets`): the batcher records
-- the event when a slot-guarded `marketplace_listing` upsert applies with `status =
-- 'SOLD_OUT'` (`crates/indexer/src/batcher.rs`, `UpsertMarketplaceAccount` arm). The IDL's
-- `sold_out` event flags stay unused (ADR-10: `emit!` events are ignored).
--
-- SHAPE -- an event table, NOT an account-state table (same exclusion as
-- `marketplace_property_metadata` ADR-27 and `webhook_events` ADR-28): no slot guard, no
-- soft close, no `db::close::StateTable` entry, no `ProgramSpec.tables` roster. The `slot`
-- column is the on-chain COORDINATE of the upsert that first showed the listing as SOLD_OUT
-- (provenance), not a mirror slot guard. A devnet reset / volume drop wipes it with
-- everything else; the crawls re-derive the rows (and the loop re-delivers any that were
-- never delivered).
--
-- EVENT IDENTITY: `event_id` is `listing_sold_out:<base58 Listing PDA>` -- one Listing PDA
-- is one primary sale (resales go through `ShareListing`, a different account), so each
-- listing is announced at most once, ever, via the `ON CONFLICT (event_id) DO NOTHING`
-- insert. WHY ONE ROW PER LISTING AND NOT PER INVESTOR: the reserver set lives in
-- `marketplace_investor_position` and changes between record and delivery (claims,
-- un-reserves, crank releases) -- the loop fans out over the CURRENT reservers at send time,
-- which is also what makes a spuriously-recorded event (a stale re-org update) harmless: the
-- loop re-validates listing status, `claim_deadline` and the reserver set before POSTing.
-- The per-row delivery state mirrors 0014:
--   * `attempts`        = consecutive failed deliveries (reset to 0 by a success);
--   * `next_attempt_at` = backoff deadline (30 s, doubling per failure, 1 h cap, computed in the
--                         failure update); NULL after a success;
--   * `last_error`      = the last failure's message; NULL after a success;
--   * `delivered_at`    = when the loop finished the event -- every current reserver got a
--                         2xx or a permanent skip (404 = wallet not registered, a normal
--                         outcome per the notifications API), or the claim window had already
--                         closed, or nobody holds a reservation any more (NULL = still
--                         pending / being retried).

CREATE TABLE sold_out_notifications (
    event_id        TEXT PRIMARY KEY,
    listing_id      BIGINT NOT NULL,
    asset_id        BIGINT NOT NULL,
    -- Slot of the account update that first showed the listing SOLD_OUT (provenance), not a
    -- mirror slot guard.
    slot            BIGINT NOT NULL,
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
CREATE INDEX sold_out_notifications_pending_idx ON sold_out_notifications (event_id)
    WHERE delivered_at IS NULL;
