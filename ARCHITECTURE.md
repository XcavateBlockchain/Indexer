# Architecture

How the indexer is put together, and why. For the "why we chose X over Y" record, see
[DECISIONS.md](DECISIONS.md) — this document is the "how it fits together" one. For
day-2 operations, see [RUNBOOK.md](RUNBOOK.md).

## 1. Pipeline

```
                    ┌─────────────────────────────┐
                    │   Alchemy Yellowstone gRPC   │   Solana devnet, commitment=confirmed
                    │  (X-Token auth, program-     │   (ADR-5)
                    │   scoped account+tx filters) │
                    └───────────────┬──────────────┘
                                    │ raw account/tx updates
                                    ▼
                    ┌─────────────────────────────┐
                    │  carbon 0.12.0 decoders,     │   crates/{whitelist,regions,
                    │  one per program (generated  │   marketplace,property}-decoder
                    │  by carbon-cli, never        │   (never hand-edited — see
                    │  hand-edited)                │    README "Regenerating")
                    └───────────────┬──────────────┘
                                    │ decoded instructions / accounts
                                    ▼
        ┌───────────────────────────────────────────────────┐
        │        crates/indexer -- Carbon pipeline           │
        │   (one pipeline; a decoder+processor pair per      │
        │    program, each decoder self-filters by id)       │
        │  InstructionProcessor<M>  AccountProcessor<M>  AccountDeletionProcessor
        │  (mapping/<program>.rs:   (account state,   (redundant close path,
        │   per-program contracts)   slot-guarded)      ADR-14; shared)
        │         │                    │                     │
        │         └────────────┬───────┴─────────────────────┘
        │                      ▼
        │              batcher.rs -- one Postgres tx per flush
        │              (≤100 ops or 250ms, phase-ordered WriteOps,
        │               FlushOutcome tracked -- ADR-15)
        └──────────────────────┬──────────────────────────────┘
                               ▼
                    ┌─────────────────────────────┐
                    │          Postgres            │
                    │  program_instructions        │  append-only history, all four
                    │                              │  programs, program_id column (§2)
                    │  config / admin / role_account│  whitelist current-state (§2)
                    │  regions_* marketplace_*      │  sibling current-state, 21 tables,
                    │  property_*                   │  same slot-guarded pattern (§2)
                    │  whitelist_actions            │  whitelist parity/audit log (§3)
                    │  admins_view / role_          │  derived, folded (§3)
                    │  assignments_view / config_view│
                    │  sync_state / backfill_cursor │  bookkeeping, one row per program
                    └───────────────┬───────────────┘
                                    │ read-only pool, 5s statement_timeout
                                    ▼
                    ┌─────────────────────────────┐
                    │   crates/api -- GraphQL       │  Axum + Juniper, :3010
                    │   (QueryRoot over the views;  │  /graphql /graphiql /health
                    │    DoS guards before execute) │
                    └─────────────────────────────┘

  Alongside the live stream (all in crates/indexer, all writing through the same batcher):
    getSignaturesForAddress crawl  -- backfill (once) + reconcile (every RECONCILE_INTERVAL)
    getProgramAccounts snapshot    -- one-shot, on a fresh/truncated database
```

Both Rust binaries (`indexer`, `api`) expose Prometheus metrics on their own `/metrics`
port (`9464`/`9465` by default — see [DECISIONS.md ADR-16](DECISIONS.md#adr-16-custom-prometheus-exporter));
Prometheus scrapes both, Grafana reads Prometheus, per [RUNBOOK.md](RUNBOOK.md).

## 2. The two-table split (spec §5)

Every account update lands in **two** kinds of table, for different reasons:

- **`program_instructions`** — append-only, one row per decoded instruction of any of the
  four programs (including nested/CPI instructions, collapsed to `(outer, inner)` index
  pairs), attributed by its `program_id` column, `ON CONFLICT DO NOTHING`. This is the raw
  history: every instruction the programs ever executed, in full, with JSONB-encoded
  arguments (`ADR-4`). Nothing downstream trusts insertion order — a reprocessed
  instruction is a no-op.
- **The account-state tables** — slot-guarded, current-only account state, one table per
  on-chain account type, one row per PDA (`ADR-2`, `ADR-6`): the whitelist's legacy
  unprefixed `config` / `admin` / `role_account` plus the program-prefixed sibling tables
  (`regions_*`, `marketplace_*`, `property_*` — 21 tables, ADR-22). These hold *only*
  fields that exist on-chain, so they stay droppable and rebuildable from fresh
  `getProgramAccounts` snapshots at any time (`ADR-3`) — a close is a soft `UPDATE`, never
  a `DELETE` (`ADR-7`).

Why both exist rather than just one: `program_instructions` is what makes a full
`indexer backfill` replay-safe and auditable (it is where `whitelist_actions`, §3, is
derived from); the account-state tables are what makes "what does the config PDA say right
now" a single indexed row lookup instead of a fold over the entire instruction history on
every query.

## 3. The parity layer: `whitelist_actions` + fold views

The old SubQuery schema exposed `Config`/`Admin`/`RoleAssignment` as single soft-deleted
rows carrying both on-chain fields *and* derived audit fields (`active`, `addedBy`,
`removedAt*`, `removalKind`, …) mutated in place as instructions were indexed, in whatever
order they arrived.

This indexer keeps the same GraphQL-visible shape (see the rename table in
[DECISIONS.md ADR-11](DECISIONS.md#adr-11-divergences-from-the-old-schemagraphql)) but
computes it differently:

- **`whitelist_actions`** — append-only, one row per instruction, same identity
  (`<txSignature>-<ixPath>`) and the old `ActionType` taxonomy, written by the same
  instruction processor that writes `program_instructions`.
- **`admins_view` / `role_assignments_view` / `config_view`** — SQL views that **fold**
  over `whitelist_actions` (and, for `config_view`, join the `config` state table for the
  on-chain fields), sorted internally by a canonical order
  (`slot, block_time, tx_signature, ix_path`) rather than by insertion order.

**Why folded, not incrementally mutated** (`ADR-13`): this indexer has two writers of
instruction history that don't share a single monotonic order — the live stream (newest
first) and a backwards history walk (also newest-to-oldest, running independently and
possibly concurrently). In-place mutation is order-*sensitive* — applying the same two
events in opposite order can leave a different final row. A fold that always sorts its
inputs before computing state is order-*insensitive by construction*: it doesn't matter
whether the row for slot 100 or the row for slot 200 was inserted first, the view always
reads them out in the canonical order and converges to the same answer once backfill
completes.

Cost: view SQL complexity — see `migrations/0005_derived_views.sql`, whose own comments
carry the ordering contract in full. If data volume ever grows enough that folding at read
time becomes expensive, the fix is materialized views with an explicit refresh, not a
change to the ordering logic.

## 4. Backfill ordering: stream first → snapshot → history walk

On `indexer run` startup, in this order (`crates/indexer/src/main.rs`):

1. **Subscribe gate** — connect to Yellowstone with the production filters and wait for a
   first update (a slot heartbeat, ~400ms even on an idle program) before doing anything
   else. A rejected key exits the process with code 1 instead of hot-looping invisibly
   inside carbon's datasource.
2. **Live stream subscribed.** Freshness starts here.
3. **`STREAM_SETTLE` (5s)**, then, if needed:
4. **Snapshot** (`getProgramAccounts`, only if `sync_state.snapshot_slot IS NULL`).
5. **History backfill** (only if `sync_state.backfill_complete = false`).

**Why this exact order** (`ADR-8`): `getProgramAccounts` takes noticeable wall-clock time.
Any account that changes between the snapshot's read and the stream's subscription would be
invisible to *both* if the snapshot ran first — a permanent hole exactly as wide as the
snapshot. Subscribing first makes the stream and the snapshot overlap instead of race, and
the slot guard (`ADR-6`) resolves the overlap safely: whichever write has the higher slot
wins, so it never matters which one technically landed in Postgres first.

The history backfill is independent of both: it's a `getSignaturesForAddress` walk from the
chain tip down to `sync_state.backfill_floor_slot` (the program's deployment slot), feeding
every transaction through the exact same instruction pipe and batcher the live stream uses.
It's resumable (`backfill_cursor`, one singleton row, written through the batcher so it can
never claim a page whose rows didn't commit) and safe to re-run to completion at any time —
every write on this path is idempotent.

## 5. Contiguity: crawler-driven reconciliation

**The question this answers**: "is there a gap in the indexed history below slot T?" — not
"is the latest transaction in the database," which the stream already answers on its own.

**Why the stream can't answer it** (`ADR-15`): carbon's Yellowstone datasource
re-subscribes *internally* on a stream error and swallows plan/auth rejections inside a
spawned task, so a process cannot observe that it briefly missed a window. And on a program
that's idle for days at a time, "no updates received" is *also* the normal, healthy case —
so silence proves nothing about contiguity either way.

**The division of labour**:

| Path | Owns | Mechanism |
| --- | --- | --- |
| Yellowstone gRPC stream | **freshness** | a new transaction is in the DB in ~1s |
| `getSignaturesForAddress` crawl (`indexer backfill`, then the periodic reconciliation supervisor) | **completeness** | `sync_state.last_contiguous_slot` — "nothing below this slot is missing," proven by having actually walked and re-indexed that range |
| `getProgramAccounts` snapshot (`indexer snapshot`) | **current state** on idle programs | the account-state tables (per program), each program's `sync_state.snapshot_slot`; also sweeps stale-open rows closed (accounts absent from the snapshot are provably gone) |

The reconciliation supervisor is the *only* writer of `last_contiguous_slot`: every
`RECONCILE_INTERVAL` (default 300s), it reads `getSlot` **before** crawling (so its claim
only covers a range it actually walked), crawls newest-to-oldest down to
each program's `last_contiguous_slot + 1`, and only then advances that program's frontier to
the pre-crawl tip (one shared `getSlot` per tick serves all four programs; each enumeration
page additionally proves the serving node's view has reached that tip, so a lagging fallback
node cannot cause a false contiguity claim). On quiet programs this is one page (~2 RPC
calls) per program per cycle — about 2,880 requests/day for four programs, under 6% of
Alchemy's free-tier budget.

`chain_tip_slot - last_contiguous_slot` (the Grafana slot-lag panel, the `SlotLagHigh`
alert) is therefore an evidence-backed freshness claim, not a hopeful one.

## 6. Where the old SubQuery logic ended up

| Old (`src/mappings/mappingHandlers.ts`) | New |
| --- | --- |
| `metaOf(ix)` (`txSignature`, `blockHeight`, `blockTime`, `instructionIndex`) | `mapping::map_instruction` reads carbon's `InstructionMetadata`/`TransactionMetadata`; `blockHeight` → `slot` (ADR-11), `instructionIndex` is `mapping::instruction_index(absolute_path)` — same dot-joined format |
| `accountAt(ix, n)` (key-index indirection through static + lookup-table keys) | `mapping::account_at(decoded.accounts, n)` — carbon has already resolved static + lookup-table keys, so there's no indirection left to redo |
| `decodedArgs(ix)` | the generated `InstructionDecoder`; instructions that fail to decode never reach the mapper |
| `recordAction(...)` | one `WriteOp::InsertAction` per instruction, pushed to the batcher |
| `Config.create/save`, `Admin.create/save`, `RoleAssignment.create/save` (order-sensitive in-place mutation) | **gone** — replaced by `whitelist_actions` + the fold views (§3, ADR-13). Account state comes from the account pipe instead, straight off chain, never derived from instructions |
| `invariant(...)` throwing to halt indexing on a violated assumption | `mapping::MappingError` → `decode_skipped_total{reason}` counter + an error log + a failed carbon update (`updates_failed`). Same stance as the old handlers: data integrity over liveness — the *processing* of a bad instruction fails loudly, the process itself keeps running |
| The 3 instructions that close a PDA (`remove_admin`, `remove_role`, `renounce_role`) setting `active = false` on the entity | additionally emit a `WriteOp::CloseAccount` from the **instruction** processor (ADR-14), not just the deletion pipe — the sibling programs' closing instructions follow the same pattern (see `crates/indexer/src/mapping/`) |
| Handler-level account-position lookups (`authority=accounts[0]` everywhere; `new_admin`/`user = accounts[2]` for `add_admin`/`assign_role`/`remove_role`/`set_permission`; `renounce_role` user `= accounts[0]`) | same positions, ported verbatim into `mapping.rs`, asserted by one test per instruction variant against the brief's contract table |
| Events (`WhitelistAction`'s payload also recoverable from logs) | still ignored (ADR-10) — the generated decoder's `events/` module exists but is never wired into a processor |

## 7. Repository layout

```
crates/
  whitelist-decoder/    generated by carbon-cli, one crate per program's IDL — never hand-edited (README §"Regenerating")
  regions-decoder/
  marketplace-decoder/
  property-decoder/
  indexer/              the Carbon pipeline binary: run / backfill / snapshot / smoke-grpc
  api/                  the GraphQL API binary (Axum + Juniper), port 3010
migrations/             sqlx migrations, applied in filename order (0001..0010)
idls/                   the four programs' Anchor IDLs — all indexed (ADR-22)
monitoring/              Prometheus scrape config + alert rules + Grafana provisioning/dashboards
docker/                 rust.Dockerfile (indexer+api), pg-Dockerfile (postgres), node.Dockerfile (SubQuery rollback only)
docker-compose.yml       the active stack: postgres, indexer, api, prometheus, grafana
docker-compose.subquery.yml   the disabled SubQuery rollback stack (ADR-21)
.github/workflows/       ci.yml, deploy.yml — adapted, not replaced (ADR-21)
grpc-api/                old gRPC read API — unwired, rollback-only (ADR-18)
src/, schema.graphql, project.ts, project.yaml   old SubQuery indexer source — kept for the rollback stack
```
