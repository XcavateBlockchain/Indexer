-- Version boundaries of the indexed programs, as observed on-chain (ADR-24).
--
-- One row per (program, slot at which a new bytecode version became live). Two writers, both
-- idempotent (`ON CONFLICT DO NOTHING` -- replay-safe like every append-only table here):
--
--   * startup seeding (`db::upgrades::seed_deploy_slot`, called from `main::start` exactly
--     like `sync_state`'s seed): each configured program's compiled-in deploy slot, with
--     source 'deploy'. That is the slot the program's version-1 bytecode became live, so the
--     table always describes the FULL version timeline, not just the upgrades.
--   * the loader recorder pipe (`crates/indexer/src/upgrades.rs`): every successful
--     BPFLoaderUpgradeable `Upgrade` instruction targeting a registry program, with source
--     'chain'. It fires on the live stream AND on backfill/reconcile crawls (upgrade
--     transactions reference the program account, so the existing per-program filters and
--     `getSignaturesForAddress` walks already deliver them -- no new subscription), which
--     means a full backfill re-walk also heals any upgrade missed during downtime.
--
-- Append-only, never UPDATE or DELETE: a boundary is a historical on-chain fact. The rows are
-- the slot-boundary registry that versioned decoding (designed but dormant -- see
-- docs/agentic-maintenance.md and ADR-25) routes on: version N of a program owns slots
-- [boundary_N, boundary_N+1). Until a second decoder version exists, the table is
-- observability only: a 'chain' row appearing is the signal that the checked-in IDL and the
-- deployed program may have diverged (surfaced by the ProgramUpgradeDetected alert and a
-- startup warning).
--
-- After a devnet ledger reset this table is orphaned along with everything else and is wiped
-- and re-seeded by the same volume-drop + fresh-start procedure (RUNBOOK.md "Devnet ledger
-- reset").
CREATE TABLE program_upgrades (
    program_id   BYTEA NOT NULL,
    -- Slot of the deploy / upgrade transaction. Activity at this exact slot is unorderable
    -- against the upgrade itself at our granularity (same caveat as db::close's same-slot
    -- tie); the routing rule for the boundary slot lives with the router, not here.
    upgrade_slot BIGINT NOT NULL,
    -- base58 signature of the transaction carrying the Upgrade instruction. NULL for seeded
    -- 'deploy' rows: the deploy slots are compiled-in recon facts, their signatures were
    -- never recorded.
    signature    TEXT,
    source       TEXT NOT NULL CHECK (source IN ('deploy', 'chain')),
    detected_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (program_id, upgrade_slot)
);

-- Which decoder version produced a program_instructions row's `data` JSONB. NULL everywhere
-- until versioned decoding activates (ADR-25): with a single decoder per program the version
-- is unambiguous, and the column exists now so activating versioning later is an additive
-- code change, not a schema migration racing an on-chain upgrade. Consumers of `data` can
-- treat NULL as "version 1 -- the only decoder that existed when the row was written".
ALTER TABLE program_instructions ADD COLUMN decoder_version SMALLINT;
