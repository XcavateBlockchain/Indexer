-- Resume cursor for the historical transaction backfill (`indexer backfill`).
--
-- The backfill walks `getSignaturesForAddress(PROGRAM_ID)` newest -> oldest in pages. A page is
-- only "done" once every transaction in it has been fetched, mapped and COMMITTED; at that
-- point the oldest signature of the page is written here, and a resumed run passes it as
-- `before` so the walk continues below it instead of starting from the chain tip again.
--
-- The cursor is an optimisation, not a correctness mechanism: every write the backfill makes is
-- idempotent and slot-guarded, so an interrupted run can always be re-run from scratch. What
-- the cursor buys is RPC budget (Alchemy's free tier is finite) on a resume.
--
-- Singleton, like `sync_state`: one indexer instance, one backfill walk. The CHECK keeps that
-- true at the schema level instead of by convention.
--
-- Deleted when a walk reaches its stop condition (floor or end of history): "a cursor exists"
-- then means exactly "an interrupted walk is waiting to be resumed", and a re-run of a
-- completed backfill starts from the tip and re-verifies the whole range.
CREATE TABLE backfill_cursor (
    id         SMALLINT PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    -- base58, as returned by getSignaturesForAddress and as accepted by `before`.
    signature  TEXT NOT NULL,
    -- Slot of that signature. Not used to resume (the RPC needs the signature, not the slot);
    -- kept because an operator looking at this table wants to know how far down the walk got.
    slot       BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
