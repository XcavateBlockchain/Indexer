---
name: write-migration
description: Add a Postgres migration (new state table, new column, widened enum CHECK, view change, index) plus its Rust lockstep changes, as part of an indexer-update PR. Use whenever a decoder/mapper change needs a schema change. Never use it to edit an existing migration file.
---

# Write a database migration

**Use when** an indexer update needs a schema change: a program upgrade added an account
type (new state table), added a field (new column), added an enum variant (widened CHECK),
or a new API resolver needs an index or a changed view.

**Do NOT use when** the change can be absorbed without SQL (JSONB `data` in
`program_instructions` already holds any new instruction args — that table needs no
migration for new instructions), or when you are tempted to edit a file already under
`migrations/` — applied migrations are immutable, full stop (sqlx checksums them; editing
one crash-loops every existing database, including production, at its next startup).

## Procedure

1. **Number and name.** Next 4-digit number, strictly increasing, `NNNN_snake_case.sql`
   (`scripts/lint-migrations.sh` rejects anything else):

   ```bash
   ls migrations/ | sort | tail -1    # currently 0011_program_upgrades.sql -> yours is 0012_*
   ```

2. **Write the header comment first.** Read `migrations/0008_regions_state.sql` and
   `migrations/0011_program_upgrades.sql` headers and copy their style: state the invariant
   the tables carry (slot-guarded current-only / append-only), the ADR refs, and WHY each
   non-obvious shape decision was made. The header is the contract future migrations are
   reviewed against.

3. **Stay additive.** `scripts/lint-migrations.sh` (run locally and by ci.yml's
   `migration-lint` job) fails the PR on: any modify/delete/rename of an existing
   migration, and any of `DROP TABLE`, `DROP COLUMN`, `ALTER COLUMN`, `RENAME`, `DELETE`,
   `TRUNCATE`, `UPDATE` in a new file. If one of those is genuinely required, the bar is
   0007's: a written correctness argument in the header (0007's backfill `UPDATE`s were
   correct only because that migration provably ran before any sibling-program row could
   exist), plus a per-keyword unlock marker in the file:

   ```sql
   -- lint: allow UPDATE -- every pre-existing row belongs to the whitelist program (see header)
   ```

   `DROP VIEW` / `CREATE OR REPLACE VIEW` are always allowed (views are derived, not data).

4. **Type mapping** (canonical statement: 0008's header; JSONB/flattening: 0009/0010 headers):

   | on-chain                | column |
   |-------------------------|--------|
   | `u8`                    | `SMALLINT` |
   | `u16`                   | `INT` (u16 max 65,535 exceeds SMALLINT; never rely on on-chain bounds) |
   | `u32`/`u64`/`i64`       | `BIGINT` (u64 above i64::MAX wraps negative via the mapper's `as i64` — accepted caveat, same as `lamports`) |
   | `bool`                  | `BOOLEAN` |
   | `Pubkey`                | `BYTEA` |
   | `Option<T>`             | nullable column of T's mapping |
   | enum                    | `TEXT` + `CHECK (col IN (...))`, SCREAMING_SNAKE spellings, with a `-- Borsh variant order X/Y/Z (0/1/2) is load-bearing.` comment |
   | `bytes` (byte string)   | `BYTEA` (0008's `postcode`), not JSONB |
   | genuine `Vec<T>` lists  | `JSONB` in a shape the INDEXER constructs itself — NEVER the decoder's serde output (decoder regeneration must not silently change stored shapes; see 0010's `locations`) |
   | fixed nested struct     | flattened `<field>_*` columns (0010's `election_*`) |

5. **THE STALL TRAP — widen enum CHECKs BEFORE the new decoder can write the variant.**
   If an on-chain enum gained a variant and the CHECK is not widened, the first row
   carrying it violates the CHECK; `crates/indexer/src/batcher.rs` retries the identical
   failed batch forever (exponential backoff, 1s→30s cap, until shutdown), the bounded
   channel backpressures the processor, and carbon's update loop halts — ingestion for ALL
   FOUR programs stalls on one deterministic failure. Postgres cannot alter a CHECK in
   place; the sanctioned pattern is drop+re-add with a strict-superset argument:

   ```sql
   -- Widen regions_region_state.status for the new CANCELLED variant (borsh index 3, appended
   -- upstream in <commit>). Safe because the new set is a strict superset of the old: every
   -- existing row satisfies the new constraint, so the swap cannot fail mid-apply, and the old
   -- decoder never emits the new spelling.
   ALTER TABLE regions_region_state DROP CONSTRAINT regions_region_state_status_check;
   ALTER TABLE regions_region_state ADD CONSTRAINT regions_region_state_status_check
       CHECK (status IN ('PROPOSING', 'PASSED', 'REJECTED', 'CANCELLED'));
   ```

   Find the real constraint name first (default is `<table>_<column>_check`, but verify):

   ```bash
   docker exec carbon-mig-test-pg psql -U postgres -c \
     "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint
      WHERE conrelid = 'regions_region_state'::regclass AND contype = 'c';"
   ```

   Note: the lint's keyword list does NOT include `DROP CONSTRAINT`, so this pattern trips
   nothing mechanically — the header's superset argument is the ONLY guardrail. Write it
   anyway, at 0007's bar. Extend the matching Rust enum's `as_db_str` in
   `crates/indexer/src/db/<program>.rs` in the same PR (e.g. `Vote`/`RegionStatus` in
   `db/regions.rs`). Ordering: this migration ships in the SAME PR as the decoder change,
   and migrations auto-apply at indexer startup (`db::run_migrations` from `main::start`;
   docs/deployment.md) — there is no deploy-time migration gate, a bad migration
   crash-loops the indexer container, and deploy.yml's "Verify deployment" step probes only
   api/prometheus/grafana health, NEVER the indexer container. A broken migration deploys
   "green".

6. **New state table** (new account type). Column contract, exactly 0002/0008:
   `pubkey BYTEA PRIMARY KEY`, `slot BIGINT NOT NULL` (the write guard),
   `lamports BIGINT NOT NULL`, `closed_at_slot BIGINT` (NULL = live), then ON-CHAIN FIELDS
   ONLY (ADR-2/3: the table must stay droppable and rebuildable from a
   `getProgramAccounts` snapshot). Name it `<program>_<entity>` (whitelist's three keep
   legacy unprefixed names). Rust lockstep, all in the same PR:
   - Row struct + slot-guarded upsert in `crates/indexer/src/db/<program>.rs`:
     `INSERT ... ON CONFLICT (pubkey) DO UPDATE SET <every column> WHERE <table>.slot < EXCLUDED.slot`.
     The `DO UPDATE SET` list MUST name EVERY column — an omitted column silently keeps its
     stale value on every conflict-path update, with no error, ever.
   - Add the variant to `StateTable` AND `StateTable::ALL` AND `table_name()` in
     `crates/indexer/src/db/close.rs` (the generic soft close and the deletion-tracker
     seed are dynamic SQL over this enum).
   - Add it to the owning program's `tables` list in `crates/indexer/src/programs.rs`
     (the `every_state_table_belongs_to_exactly_one_program` test enforces the partition —
     a `StateTable::ALL` entry in zero or two lists fails the build's tests).
   - Add an arm to the program's `<Program>AccountRow` enum + `upsert` dispatch in
     `db/<program>.rs`. A new `WriteOp` variant in `batcher.rs` only for a genuinely new
     KIND of write (compare `CloseLettingAgentIfLast`) — a new account type rides the
     existing `Upsert<Program>Account` variant.

7. **New column on an existing table**: nullable or `DEFAULT`-ed (a bare `NOT NULL` add
   fails on non-empty tables, and a backfill `UPDATE` needs the 0007-bar argument). Then
   the same completeness trap: add it to the row struct, the INSERT column list, AND the
   `DO UPDATE SET` list in `db/<program>.rs` — forgetting the third compiles and passes a
   fresh-insert test while rotting every subsequent update.

8. **Views** (`migrations/0005_derived_views.sql`): any change = `DROP VIEW x;` then full
   `CREATE VIEW x AS ...` in the NEW migration (`CREATE OR REPLACE` can only append
   trailing columns — it cannot drop, reorder, or retype). The lint always allows this.
   `role_assignments_view` hardcodes the role→borsh-index mapping in a SQL `CASE`
   (0005 ~lines 152–159): a new role variant needs that CASE extended or the affected rows'
   `id` goes NULL outright (`text || '-' || NULL` is NULL in SQL) — silently, no error.

9. **Indexes**: every column a new/changed API resolver filters or orders by gets one
   (0008's `idx_<table>_<col>` naming). The api pool sets `statement_timeout = '5s'` on
   every connection (`crates/api/src/db.rs`) — an unindexed scan that outgrows 5s is a
   production 500, not a slow query.

10. **Tests** (`crates/indexer/src/db/tests.rs`; copy the patterns under its `====` section
    banners, all `#[sqlx::test(migrations = "../../migrations")]`):
    - Per new state table, a mandatory `slot_guard_*` test: upsert at slot 200, then
      slot 100, assert the row still reads slot 200 (model: `slot_guard_holds_on_a_sibling_table`).
      The existing `the_generic_close_matches_every_state_table` covers your new
      `StateTable` entry automatically — but ONLY if step 6 added it to `ALL`.
    - Per new append-only table, an idempotency test (model: `insert_instruction_is_idempotent`,
      and the `ON CONFLICT DO NOTHING` tests in the 0011 section) — replay-safety is the
      backfill contract.

11. **Regenerate both .sqlx caches** against the migrated long-lived compile-check pg,
    FROM INSIDE each crate dir (root-level `--workspace` prepare does not work here —
    ci.yml documents why):

    ```bash
    docker start carbon-mig-test-pg 2>/dev/null || \
      docker run -d --name carbon-mig-test-pg -e POSTGRES_PASSWORD=test -p 54329:5432 postgres:16
    export DATABASE_URL=postgres://postgres:test@localhost:54329/postgres
    sqlx migrate run --source migrations          # sqlx-cli 0.8.6; incremental, idempotent
    (cd crates/indexer && cargo sqlx prepare -- --lib)
    (cd crates/api && cargo sqlx prepare -- --bin api)
    ```

    Commit the changed files under `crates/indexer/.sqlx/` and `crates/api/.sqlx/`.

12. **Full local gauntlet** (mirrors ci.yml's gates), then hand off:

    ```bash
    bash scripts/lint-migrations.sh
    cargo fmt                                       # NEVER `cargo fmt --all` (README ~150: it reformats the generated decoder crates)
    cargo clippy --workspace --all-targets -- -D warnings
    SQLX_OFFLINE=true cargo build --workspace --locked
    cargo test --workspace --locked                 # needs DATABASE_URL from step 11
    (cd crates/indexer && cargo sqlx prepare --check -- --lib)
    (cd crates/api && cargo sqlx prepare --check -- --bin api)
    bash scripts/agent/verify-devnet.sh             # full rebuild-from-devnet into a throwaway pg (port 54331)
    ```

    Then read `agent/skills/verify-and-ship/SKILL.md` and follow it to open the PR — never
    push to main (main auto-deploys to production via `.github/workflows/deploy.yml`).

## Checklist before you finish

- [ ] New file only, `NNNN_` strictly above the current max; zero edits to existing `migrations/` files.
- [ ] Header comment: invariant + ADR refs + why, 0008/0011 style; correctness argument for any lint-marked or CHECK-swap statement.
- [ ] Every enum the decoder can now emit fits its CHECK (grep the new decoder's variants against the migration).
- [ ] `DO UPDATE SET` lists every column of every touched upsert.
- [ ] `StateTable` enum + `ALL` + `table_name()` + `programs.rs` `tables` all updated for any new table.
- [ ] `slot_guard_*` test per new state table; idempotency test per new append-only table.
- [ ] Both `.sqlx/` caches regenerated from inside their crate dirs and committed.
- [ ] `lint-migrations.sh`, fmt/clippy/build/test, both `prepare --check`, `verify-devnet.sh` all green.
- [ ] Migration + decoder + mapper + Rust lockstep in ONE PR (never a migration trailing a decoder that already writes the new shape).

## Traps

- **Editing an applied migration**: sqlx checksum mismatch → instant crash-loop for every existing DB including production. Also: if you renumber/edit your OWN in-progress file after applying it to carbon-mig-test-pg, that local DB is poisoned — recreate the container; never "fix" by editing history.
- **CHECK stall**: an unwidened enum CHECK deterministically fails the batch; `batcher.rs` retries it forever and all four programs stop ingesting. The lint will NOT warn you — it has no `DROP CONSTRAINT`/`ADD CONSTRAINT` keyword.
- **DO UPDATE SET omission**: compiles, passes insert tests, silently serves stale values on every conflict update. Grep each touched upsert: column count in `SET` must equal columns-minus-pubkey.
- **Roster drift**: `the_generic_close_matches_every_state_table` catches a `StateTable` entry with no real table (dynamic UPDATE errors); `every_state_table_belongs_to_exactly_one_program` catches an entry missing from (or duplicated across) `ProgramSpec.tables`. NOTHING catches a table created in SQL but never added to `StateTable` — it passes every test and is simply never closed or swept. The enum edit is on you.
- **Version metadata in state tables**: decoder/schema-version columns belong ONLY on append-only history (`program_instructions.decoder_version`, 0011). State tables are ADR-2 disposable on-chain mirrors — anything not reconstructible from a fresh snapshot breaks their drop-and-rebuild contract.
- **Migrations run at STARTUP, not deploy**: merge → auto-deploy → indexer container applies your SQL unattended; deploy.yml's verify never probes the indexer container, so a crash-looping migration looks like a green deploy. `verify-devnet.sh` (migrations from zero) is your real gate — run it.
- **Decoder serde shapes in JSONB**: storing decoder output verbatim ties stored data to a generated crate's serde details; regeneration then silently changes history. Construct JSONB shapes in the mapper (0010's `locations` pattern).
