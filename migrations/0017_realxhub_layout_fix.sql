-- Migration 0017 — realxhub deployed-layout fix (ADR-30 addendum 2026-09-03).
--
-- The realxhub program landed on devnet (first deploy slot 489635042) with a state
-- layout the pre-deploy IDL that 0015 was written from never saw: the deployed
-- `Holding` account carries a leading `hub_id: u64` + `owner: Pubkey` before the
-- fields 0015 modeled, and the deployed `ShareListing` account carries a leading
-- `hub_id: u64`. Until this migration, rows decoded from the deployed accounts
-- were shifted: `holder`/`seller` held the wrong pubkey bytes (garbage), and
-- `amount`/`price` were misread. `idls/realxhub.json` now matches the deployed
-- layout (the chain outranks the repo — hard rule 6) and these columns match the
-- corrected decoders.
--
-- The `NOT NULL DEFAULT`s are a temporary lie over the already-poisoned rows:
-- existing rows keep their garbage `holder`/`seller` bytes with `hub_id = 0` and
-- the all-zero `owner` until the next full reindex. `indexer snapshot` (or
-- backfill) re-upserts every account at a newer slot, and the
-- `WHERE slot < EXCLUDED.slot` guard (0002's pattern) accepts those writes, which
-- heals the whole table. The reindex is the repair; do not hand-correct rows.
--
-- Additive-only: three ADD COLUMNs and three CREATE INDEXes; no existing column,
-- row, or constraint is touched.

-- realxhub ShareListing (deployed): { hub_id: u64, seller: Pubkey, amount: u32, price: u64, bump: u8 }
ALTER TABLE realxhub_share_listing ADD COLUMN hub_id BIGINT NOT NULL DEFAULT 0;

-- realxhub Holding (deployed): { hub_id: u64, owner: Pubkey, amount: u32, listed: u32,
-- per_share: u128, pending: u64, bump: u8 }
ALTER TABLE realxhub_holding ADD COLUMN hub_id BIGINT NOT NULL DEFAULT 0;
ALTER TABLE realxhub_holding ADD COLUMN owner BYTEA NOT NULL DEFAULT '\x0000000000000000000000000000000000000000000000000000000000000000'::bytea;

CREATE INDEX idx_realxhub_share_listing_hub_id ON realxhub_share_listing (hub_id);
CREATE INDEX idx_realxhub_holding_hub_id ON realxhub_holding (hub_id);
CREATE INDEX idx_realxhub_holding_owner ON realxhub_holding (owner);
