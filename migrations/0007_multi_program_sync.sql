-- Multi-program support, step 1 of 2: make the shared/bookkeeping tables program-aware.
-- (Step 2 is the per-program account-state tables in 0008..0010.)
--
-- Until now the whole database was implicitly scoped to the one whitelist program
-- (`2vVARM46pPD4rcHdbXHnYA4vTGN14q6skQAzsQWcHUxn`, hex 1c8f...e1ff below): the sibling
-- programs (marketplace, property, regions -- ADR-19) get indexed by the same process, so
-- everything that was "the" singleton becomes "one row per program".
--
-- Ordering note: this migration must run before any sibling-program row is ever written, so
-- the `DEFAULT`/`UPDATE ... SET program_id = <whitelist>` backfills below are correct by
-- construction -- every pre-existing row belongs to the whitelist program, because the
-- whitelist program was the only thing the indexer could write.

-- --- program_instructions: attribute every instruction row to its program -----------------
--
-- The composite PK (signature, ix_index, inner_index) stays: one instruction position in one
-- transaction belongs to exactly one program, so rows from different programs can never
-- collide on it. What DOES need the discriminator is attribution and name-scoping: all four
-- programs are Anchor programs with overlapping instruction names (`initialize_config`,
-- `update_authority`, `accept_authority`, ...), so an un-namespaced `ix_name` lookup would
-- conflate them.

ALTER TABLE program_instructions ADD COLUMN program_id BYTEA;
UPDATE program_instructions
    SET program_id = '\x1c8f502a4ae7116b0b6efde2b8fc7b5c201adfe6090fd3019b7bb4e191f4e1ff';
ALTER TABLE program_instructions ALTER COLUMN program_id SET NOT NULL;

-- Replaces idx_pi_name_time: a bare (ix_name, block_time) index would interleave the four
-- programs' identically-named instructions.
DROP INDEX idx_pi_name_time;
CREATE INDEX idx_pi_program_name_time
    ON program_instructions (program_id, ix_name, block_time DESC);

-- --- sync_state: one row per program ------------------------------------------------------
--
-- Each program deployed at its own slot and backfills independently, so
-- last_contiguous_slot / backfill_complete / backfill_floor_slot / snapshot_slot are
-- per-program facts. The pre-existing singleton row (id = 1) is adopted as the whitelist
-- program's row, preserving its progress (a re-keyed production database must NOT re-backfill
-- or re-snapshot the whitelist program).

ALTER TABLE sync_state ADD COLUMN program_id BYTEA;
UPDATE sync_state
    SET program_id = '\x1c8f502a4ae7116b0b6efde2b8fc7b5c201adfe6090fd3019b7bb4e191f4e1ff';
ALTER TABLE sync_state ALTER COLUMN program_id SET NOT NULL;
ALTER TABLE sync_state DROP CONSTRAINT sync_state_pkey;
ALTER TABLE sync_state DROP COLUMN id;  -- drops its CHECK (id = 1) with it
ALTER TABLE sync_state ADD PRIMARY KEY (program_id);

-- The sibling programs' rows are NOT inserted here: the indexer seeds them at startup
-- (db::sync_state::init_sync_state, ON CONFLICT DO NOTHING) from its compiled-in per-program
-- deploy slots, exactly as it always seeded the whitelist row.

-- --- backfill_cursor: one resume cursor per program ---------------------------------------
--
-- Four independent `getSignaturesForAddress` walks need four independent resume points; a
-- shared row would let one program's walk clobber another's. A pre-existing cursor row (an
-- interrupted whitelist walk) is adopted as the whitelist program's cursor.

ALTER TABLE backfill_cursor ADD COLUMN program_id BYTEA;
UPDATE backfill_cursor
    SET program_id = '\x1c8f502a4ae7116b0b6efde2b8fc7b5c201adfe6090fd3019b7bb4e191f4e1ff';
ALTER TABLE backfill_cursor ALTER COLUMN program_id SET NOT NULL;
ALTER TABLE backfill_cursor DROP CONSTRAINT backfill_cursor_pkey;
ALTER TABLE backfill_cursor DROP COLUMN id;  -- drops its CHECK (id = 1) with it
ALTER TABLE backfill_cursor ADD PRIMARY KEY (program_id);
