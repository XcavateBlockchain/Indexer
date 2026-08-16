# Decisions

Architecture decision records for the SubQuery → Carbon migration. Each entry is short:
context, decision, consequences. Source material and raw reasoning: `MIGRATION_LOG.md` and
the controller ledger (`.superpowers/sdd/carbon-migration-spec/progress.md`, rulings R1–R20).
Referenced from `docker-compose.subquery.yml`, `.github/workflows/ci.yml` and
`.github/workflows/deploy.yml`'s rollback comments, and from `RUNBOOK.md`.

## ADR-1: Carbon over SubQuery

**Context.** SubQuery's Solana node consumes JSON-RPC polling only; it cannot ingest a
Yellowstone/Geyser gRPC stream, and hand-writing instruction handlers in TypeScript against
a hand-authored IDL doesn't scale to indexing more than one program (three sibling IDLs —
`regions`, `marketplace`, `property` — already sit unused in `idls/`).

**Decision.** Migrate to `sevenlabs-hq/carbon`: one IDL-driven framework that generates typed
decoders for both instructions and accounts, and that natively drives a Yellowstone gRPC
datasource.

**Consequences.** Rust replaces TypeScript for the indexing path; a decoder is generated (not
hand-authored) per IDL, and adding a second program later is "drop the IDL through the same
`carbon-cli` command" rather than writing new handlers by hand. The framework itself is
version-immature (see ADR-12).

## ADR-2: Account state is current-only (disposable, snapshot-rebuildable)

**Context.** The old `Config`/`Admin`/`RoleAssignment` entities mixed on-chain fields with
derived audit fields in one soft-deleted row each. Carbon's account pipe delivers current
state on change, not history.

**Decision.** The account-state tables (`config`, `admin`, `role_account`) hold exactly the
current on-chain state of each PDA — nothing else. They are a disposable mirror: droppable
and fully rebuildable at any time from a `getProgramAccounts` snapshot (`indexer snapshot`).

**Consequences.** No migration ever has to reconcile old audit data living in these tables;
a bad row is fixed by re-snapshotting, not by a data-repair script. Audit history lives
elsewhere (ADR-3).

## ADR-3: Account-state tables hold only on-chain fields; derived values live elsewhere

**Context.** Spec §5.2 non-negotiable #3 (controller ruling R7): state and derived/audit
data must not share a row, because that's what made the old entities un-rebuildable from a
snapshot.

**Decision.** Every derived/audit field the old entities carried (`active`, `addedBy`,
`assignedAt*`, `removedAt*`, `removalKind`, …) moved into SQL views folded over the new
`whitelist_actions` append-only table, never into the state tables themselves.

**Consequences.** See ADR-13 for why the fold is a view rather than incremental mutation.
The state tables stay exactly ADR-2's disposable mirror.

## ADR-4: Typed columns for account state, JSONB for instruction payloads

**Context.** `config`/`admin`/`role_account` have a small, fixed, IDL-defined shape.
`program_instructions` stores every decoded instruction, whose payload shape varies per
instruction and isn't queried directly (the parity layer, ADR-13, is the query surface).

**Decision.** Account-state tables use typed Postgres columns (`BYTEA`, `BIGINT`, `TEXT` +
`CHECK`, …). `program_instructions.data` is `JSONB`.

**Consequences.** Account state gets compile-time-checked `sqlx::query!` reads/writes and
real constraints (borsh-index-backed `CHECK`s on `role`/`permission`). The instruction
history stays schema-flexible without a migration per instruction shape, at the cost of no
column-level typing on that one table — acceptable because nothing queries into `data`
directly; `mapping.rs` already extracted anything that needs to be typed into
`whitelist_actions`.

## ADR-5: `confirmed` commitment

**Context.** The old stack ran with `--unfinalized-blocks=true`, i.e. inherently below
`finalized`. Carbon's Yellowstone datasource takes a `CommitmentLevel`.

**Decision.** Index at `confirmed`, not `finalized`.

**Consequences.** Faster visibility (finalized commitment lags confirmed by roughly a
minute on Solana), matching the old stack's tolerance for short-lived data. Confirmed-level
rollbacks are theoretically possible on a reorg; this is a documented residual risk, the same
one the old `--unfinalized-blocks=true` setting carried. Nothing in this indexer detects or
repairs a rollback — a devnet ledger reset is handled by RUNBOOK.md's "wipe and re-backfill"
procedure, not by rollback-aware logic.

## ADR-6: Slot-guarded upserts (the stale-overwrite failure mode)

**Context.** Two independent sources write account state: the live stream and the
`getProgramAccounts` snapshot (which necessarily reads a `getSlot` in the past relative to
"now"). Without a guard, a snapshot landing after a stream update can silently overwrite
fresher data with staler data — no error, no constraint violation, just a row that quietly
goes backwards.

**Decision.** Every account-state write is
`INSERT ... ON CONFLICT (pubkey) DO UPDATE SET ... WHERE <table>.slot < EXCLUDED.slot`.
A write whose slot isn't strictly newer than what's stored is a silent no-op.

**Consequences.** The stream and the snapshot can run concurrently and overlap safely by
construction (ADR-8 depends on this). Every account write path (stream, snapshot, close)
must remember to include the guard — `crates/indexer/src/db/accounts.rs` is the one place
this is implemented, and every caller goes through it.

## ADR-7: Soft closes via `closed_at_slot`

**Context.** A closed PDA (an admin removed, a role revoked) still needs its history
queryable, and Carbon's `AccountDeletion` event carries only `{pubkey, slot,
transaction_signature}` — not enough to synthesize a full row for a pubkey never seen
created.

**Decision.** A close is a slot-guarded `UPDATE` setting `closed_at_slot` (and bumping
`slot`), never a `DELETE`. A later re-create of the same PDA at a higher slot clears
`closed_at_slot` back to `NULL`, because the normal write path always includes
`closed_at_slot = NULL` in its column list.

**Consequences.** Row history for a closed-then-reopened PDA is implicit, not versioned —
only the current state is queryable from the state tables (full audit history is
`whitelist_actions`, ADR-3). A close on an unknown pubkey is a silent no-op (nothing to
guard against); see ADR-14 for why closes are instruction-driven, not deletion-driven, in
the first place.

## ADR-8: Stream-before-snapshot ordering

**Context.** `getProgramAccounts` takes noticeable wall-clock time. Any account that changes
between the snapshot's read and the stream's subscription would be invisible to both if the
snapshot ran first: the snapshot has the pre-change value, the stream never saw the change —
a permanent hole exactly as wide as the snapshot.

**Decision.** On startup, subscribe to the live stream first, let it settle
(`STREAM_SETTLE`, 5s — not a correctness mechanism, just makes the overlap the norm instead
of a race), *then* take the snapshot. The snapshot is tagged with a `getSlot` read *before*
the `getProgramAccounts` call, so its tag can only be older than the state it describes.

**Consequences.** The slot guard (ADR-6) resolves the resulting overlap: anything the stream
already delivered at a higher slot survives the snapshot's upsert untouched. Reversing the
order would reopen the hole; this ordering is spelled out in `crates/indexer/src/main.rs`
and `crates/indexer/src/snapshot.rs` precisely because it looks like removable complexity.

## ADR-9: Carbon's built-in GraphQL (Juniper/Axum) over PostGraphile/Hasura

**Context.** The spec's default is an off-the-shelf auto-schema GraphQL engine (à la the old
stack's `subql-query`, or PostGraphile/Hasura) in front of Postgres. This migration's schema
surface is a set of folded views (not raw tables) that must keep the old `schema.graphql`
field names, plus two hand-written fields (`checkAccess`, `syncStatus`) with no table behind
them at all, plus mandatory pre-execution DoS guards (depth/complexity measurement) that must
run *before* any resolver touches the database.

**Decision.** Hand-write `crates/api`'s `QueryRoot` in Rust using `carbon_core::graphql`'s
reused primitives (`build_schema`, the `I64` scalar) over `juniper`/`juniper_axum`/`axum`,
rather than deploying PostGraphile or Hasura as a separate service.

**Consequences.** No auto-generated CRUD/filtering — every resolver and its SQL is
hand-written and hand-maintained. In exchange: full control of the schema shape (matching
the old field names exactly), a clean seam to run the guard before juniper executes
(`crates/api/src/router.rs`; `graphql_router` itself was rejected for having no such seam),
and one more Rust binary instead of one more container/service. **Revisit if**: the query
surface grows enough (generic filtering, mutations, many more tables) that hand-writing
resolvers becomes the bottleneck — at that point a PostGraphile/Hasura layer in front of the
read views becomes attractive again, since the views already present a stable relational
shape to point one at.

## ADR-10: Events are `emit!` (log-based); state derives from instructions, events ignored

**Context.** None of the four checked-in IDLs has an `event_authority` account on any
instruction, which is the tell for `emit_cpi!`; these programs use `emit!` (log-based)
events. Solana log messages are truncatable under load, unlike instruction data and account
lists. The old indexer already verified every event's payload is recoverable from its
instruction (`docs/design.md` §4).

**Decision.** Keep the old indexer's stance: index instructions, ignore events entirely.
The generated decoder's `events/` module (8 typed payload structs) is never wired into any
processor.

**Consequences.** No log-parsing code exists or is needed. If a future program's events ever
carry data not derivable from its instructions, this decision needs revisiting for that
program specifically — not a blanket change.

## ADR-11: Divergences from the old `schema.graphql`

**Context.** The old schema stored Solana **block heights** (`docs/design.md` §5); the new
one stores **slots** everywhere, because carbon and the RPC surfaces this indexer uses expose
slots natively, and the old design already flagged block-height/slot divergence by millions
on devnet as a known wart. `txSignature` remains the canonical cross-chain reference either
way.

**Decision.** Rename every `*Block`/`*AtBlock`/`blockHeight` GraphQL field to its slot
equivalent; keep every other field name, type shape, and enum spelling identical to the old
`schema.graphql`. The `Date` scalar becomes `DateTime` (juniper's `chrono` scalar) — same
field names, different scalar *type name* only.

| Old (`schema.graphql`) | New (`crates/api`'s schema) |
| --- | --- |
| `Config.updatedAtBlock` | `Config.updatedAtSlot` |
| `Admin.addedAtBlock` | `Admin.addedAtSlot` |
| `Admin.removedAtBlock` | `Admin.removedAtSlot` |
| `RoleAssignment.assignedAtBlock` | `RoleAssignment.assignedAtSlot` |
| `RoleAssignment.updatedAtBlock` | `RoleAssignment.updatedAtSlot` |
| `RoleAssignment.removedAtBlock` | `RoleAssignment.removedAtSlot` |
| `WhitelistAction.blockHeight` | `WhitelistAction.slot` |
| scalar `Date` | scalar `DateTime` (juniper `chrono`) |

Everything else — `id`, `authority`, `pendingAuthority`, `active`, `addedBy`, `user`, `role`,
`permission`, `rentPayer`, `assignedBy`, `removalKind`, `removedBy`, `type`, `subject`,
`actor`, `blockTime`, `txSignature`, `instructionIndex`, and every enum's spelling — is
name-stable.

**Consequences.** Existing GraphQL clients need to rename the `*Block(Height)` fields they
read; nothing else in a client's query shape changes. `role_account.user` was additionally
renamed to `user_pubkey` at the **database** column level only (`user` is a SQL reserved
word) — the GraphQL field stays `user`, so this has no client-visible effect.

## ADR-12: Carbon stack pinned at 0.12.0

**Context.** The recon phase verified "all carbon crates are 1.0.0 on crates.io" — true, but
the published `@sevenlabs-hq/carbon-cli` npm package (the only way to generate a decoder) was
still generating code against `carbon-core = "0.12.0"` at migration time. 0.x is
semver-incompatible across minor versions; mixing a 0.12.0-generated decoder with a
1.0.0 `PipelineBuilder` doesn't compile (two incompatible `carbon-core` copies in the
dependency graph).

**Decision.** Pin every `carbon-*` crate in the workspace (core, the Yellowstone datasource,
the transaction-crawler datasource, metrics) to exactly `0.12.0`. Re-verify every 1.0.0-era
API shape the recon had assumed, at 0.12.0, before relying on it (this surfaced real gaps —
see ADR-16, ADR-17).

**Consequences.** No `carbon-rpc-gpa-datasource` exists at 0.12.0 (ADR-17); no usable
Prometheus exporter exists at 0.12.0 (ADR-16); the Yellowstone datasource at 0.12.0 can't
express a bare `slots`/`blocks_meta` subscription (worked around in `grpc_smoke.rs`) and
re-subscribes internally on error without surfacing the gap to the pipeline (ADR-15).
**Upgrade path**: when `carbon-cli` starts generating against a newer `carbon-core`,
regenerate `crates/whitelist-decoder` and bump every other `carbon-*` pin in the same commit
— never partially upgrade.

## ADR-13: Derived state as order-insensitive fold views

**Context.** The old SubQuery handlers mutated `Config`/`Admin`/`RoleAssignment` entities
in place, in indexing order. That's fine for a strictly-increasing single-source stream, but
this indexer has two independent writers of instruction history that can arrive in either
order relative to each other: the live stream (newest first) and a backwards history walk
(also newest-to-oldest, but running concurrently and independently). In-place mutation is
order-*sensitive*: applying the same two events in the opposite order can produce a different
final row.

**Decision.** The instruction processor writes exactly two things, both order-insensitive
and idempotent: `program_instructions` (raw history) and `whitelist_actions` (append-only
parity table, one row per instruction). Every "current derived state" the old entities
exposed (`active`, `addedBy`, `removalKind`, …) is a SQL **view** that folds over
`whitelist_actions`, sorted internally by a canonical order
(`slot, block_time, tx_signature, ix_path`) — never by insertion order.

**Consequences.** It does not matter whether the row for slot 100 or slot 200 lands first;
the fold always reads them out in the same order and produces the same result. State
converges to the true value exactly once backfill completes, regardless of interleaving.
Cost: view SQL complexity (`migrations/0005_derived_views.sql`); if data volume ever grows
enough that folding on every read becomes expensive, the views would need to become
materialized views with an explicit refresh trigger — not needed at this program's volume.

## ADR-14: Instruction-driven soft closes (owner filter can't see closes)

**Context.** Carbon's Yellowstone account filter is scoped by program **owner**. The instant
an account closes, its owner changes away from the whitelist program (Solana reclaims the
lamports/rent), so the *next* update about that pubkey — the deletion notification — may
never arrive through an owner-scoped filter. Carbon's `account_deletions_tracked` mechanism
mitigates this for pubkeys the process has already seen, but a process that hasn't yet
observed a given PDA is blind to its closure via the deletion pipe alone.

**Decision.** `remove_admin`, `remove_role`, and `renounce_role` each trigger a slot-guarded
soft close of the target PDA row directly from the **instruction** processor, in the same
batch transaction as the instruction/action rows. The `account_deletions` pipe is wired up
too, as redundant belt-and-braces — same guarded write, reached by a different path.

**Consequences.** A close is guaranteed to happen even if the deletion pipe never fires for
that pubkey, because the instruction that caused the close always arrives (it's what created
the `whitelist_actions` row in the first place). Both paths converge on the identical
guarded `UPDATE`, so redundancy costs nothing — whichever arrives first wins, the second is a
no-op.

## ADR-15: Crawler-driven contiguity, `--floor`, and the flush-outcome guard

**Context.** carbon's Yellowstone datasource re-subscribes internally on a stream error
(inside its own retry loop) and swallows auth/plan rejections in a spawned task — a process
cannot observe that it briefly missed a window. On an idle program (this one, for days at a
time), "no updates received" is *also* the normal case, so silence proves nothing either way.
The stream therefore cannot be trusted to prove completeness on its own.

**Decision.** Split the two jobs explicitly: the **stream** owns freshness (a new
transaction is in the database in ~1s); a **periodic `getSignaturesForAddress` crawl**
(the reconciliation supervisor, plus the one-time `indexer backfill`) owns *completeness* —
"nothing below slot T is missing" — and is the *only* writer of
`sync_state.last_contiguous_slot`. `indexer backfill` additionally supports `--floor`, but
only claims `backfill_complete` if the walk actually reached `sync_state.backfill_floor_slot`
(a partial walk with a higher operator-supplied floor claims nothing — fix round 1 closed a
real bug here). Every commit-barrier call site (`backfill`, `reconcile`, `snapshot`, shutdown)
checks the batcher's `FlushOutcome` before writing a completion marker, so a dropped batch
(commit failure racing shutdown) can never be mistaken for a committed one.

**Consequences.** `chain_tip_slot - last_contiguous_slot` (the Grafana slot-lag panel,
`SlotLagHigh` alert) is a real, evidence-backed freshness claim, not a hope. Cost: one
`getSlot` + one `getSignaturesForAddress` page per `RECONCILE_INTERVAL` (default 300s) —
roughly 576 requests/day, under 2% of Alchemy's free-tier budget even before accounting for
the paid plan this migration otherwise uses.

## ADR-16: Custom Prometheus exporter

**Context.** `carbon-prometheus-metrics` 0.12.0 hard-binds its HTTP server to
`127.0.0.1:9100` — unreachable from another container, which every deployment topology here
requires (Prometheus scrapes `indexer`/`api` over the compose network).

**Decision.** Implement `carbon_core::metrics::Metrics` directly in `crates/indexer`
(`crates/indexer/src/metrics.rs`) on top of the `metrics` facade crate +
`metrics-exporter-prometheus`, binding `METRICS_ADDR` (default `0.0.0.0:9464`). `crates/api`
does the same independently for its own metrics, on `0.0.0.0:9465` by default so the two
binaries' exporters never collide on one host.

**Consequences.** A small bespoke metrics shim to maintain per binary, in exchange for a
metrics endpoint that's actually reachable across the container network. Counters are
pre-registered at zero on startup (including every label value) so an absent series always
means "not yet observed," never "broken."

## ADR-17: Hand-written `getProgramAccounts` snapshot loader

**Context.** No `carbon-rpc-gpa-datasource` exists at `0.12.0` (it's 1.0.0-era, per ADR-12).
Without a snapshot mechanism, the account-state tables can only be populated by the live
stream reacting to a change — impossible for a program that can sit idle for days with a
fresh (or truncated) database.

**Decision.** Write the snapshot as a plain ~40-line loop in `crates/indexer/src/snapshot.rs`
(`getSlot` → `getProgramAccounts` → the decoder → the same `account_write_op` mapping
function the live account pipe uses → the same slot-guarded upserts), not as a carbon
`Datasource`.

**Consequences.** A snapshot row and a stream-delivered row are byte-identical by
construction (same mapping function), asserted by a dedicated test. As a `Datasource` it
would have had to fabricate `AccountUpdate`s and be driven by a whole `Pipeline`, adding
complexity for no benefit over a loop that just runs to completion. Reusable if/when a
0.12.0-compatible gPA datasource ever ships, at the cost of throwing this loop away.

## ADR-18: `grpc-api` dropped; `checkAccess` preserved in GraphQL

**Context.** User decision (recon phase). The old stack's standalone gRPC service
(`grpc-api/`, port 50051) read the SubQuery-populated Postgres schema directly and exposed
`CheckAccess`/`GetConfig`/etc. Keeping a second, separately-deployed read service that has to
track this migration's new schema wasn't judged worth it, but `CheckAccess(user, role)` —
"the primary integration query," per the old design doc — was judged worth keeping
somewhere.

**Decision.** Drop `grpc-api` from the active stack (compose, CI, deploy) entirely; GraphQL
on port 3010 becomes the only query surface. Re-implement its integration query as a GraphQL
field, `checkAccess(user, role) -> AccessCheck { hasRole, compliant }`, backed by
`role_assignments_view`. `grpc-api/`'s source stays in the repo (rollback path, ADR-21) but
is wired into nothing.

**Consequences.** One fewer service to deploy, secure, and keep schema-compatible. Any
consumer of the old `CheckAccess` RPC has to switch transports (gRPC → GraphQL) but keeps
the same semantics. If gRPC itself (not just this one query) turns out to still be needed,
`grpc-api/`'s source is the starting point, not a green field.

## ADR-19: Whitelist-only scope

**Context.** User decision (recon phase). Four fresh IDLs (`xcavate_whitelist`, `regions`,
`marketplace`, `property`) were dropped into `idls/` at the start of this migration, but the
old SubQuery indexer only ever indexed `xcavate_whitelist` — the other three were already
out of scope per the old `docs/design.md` §8.

**Decision.** Migrate `xcavate_whitelist` only. The other three IDLs stay checked in,
unindexed, for a later phase.

**Consequences.** The workspace layout (per-program decoder crate under `crates/`,
`addresses.json` as the canonical address source) deliberately leaves room to add a program
later: generate its decoder the same way (ADR-1's `carbon-cli` command), give it its own
migration files, and wire a second `.instruction()`/`.account()` pair into the pipeline. Not
attempted or scaffolded here — `idls/marketplace.json`, `idls/property.json`,
`idls/regions.json` are inert data files today.

## ADR-20: Alerts as Prometheus rules only, no Alertmanager

**Context.** The old stack shipped Prometheus + Grafana with no alert routing at all.
Notification delivery (paging, Slack, email) was never in scope for this migration.

**Decision.** `monitoring/alerts.yml` defines six Prometheus alerting rules
(`SlotLagHigh`, `DecodeFailures`, `IndexerDown`, `ApiDown`, `ReconnectStorm`,
`BackfillStalled`) with no Alertmanager deployed alongside them.

**Consequences.** Firing alerts are visible on Prometheus's own `/alerts` page and through
Grafana's Prometheus datasource, but nothing pages anyone. Adding Alertmanager (and routing
config) is a self-contained follow-up whenever notification delivery becomes a real
requirement — it slots in without changing any rule.

## ADR-21: SubQuery rollback path preserved

**Context.** Spec §13: the migration must be backable-out, not just forward-only. Deleting
the old stack outright would make a rollback a `git revert` archaeology exercise instead of a
config flip.

**Decision.** Keep the old SubQuery stack fully intact, inert by default:
- `docker-compose.subquery.yml` — the pre-migration compose file, `git mv`'d unchanged
  (byte-identical apart from a top-of-file "disabled rollback path" comment) from the old
  `docker-compose.yml`. Not run by anything; `docker compose -f docker-compose.subquery.yml
  config` still parses, verified in Task 7.
- `.github/workflows/ci.yml` / `deploy.yml` — the old SubQuery/`grpc-api` build-and-deploy
  steps are commented out (not deleted), each with a `# SubQuery rollback path — disabled,
  see DECISIONS.md` marker.
- `grpc-api/` — source stays in the repo, unwired (ADR-18).
- The Postgres `pgdata` volume is **shared** between the two stacks: the new stack's tables
  live in the `public` schema; the old SubQuery/`grpc-api` tables live in schema `app`. Both
  coexist in the same volume without conflict.

**Consequences.** Rolling back is "stop the new stack, `docker compose -f
docker-compose.subquery.yml up -d`, uncomment the old CI/deploy steps" — the old data is
already sitting in `app`, untouched, as long as the volume was never wiped. See
`RUNBOOK.md`'s "Rolling back to SubQuery" for the exact steps. Cost: `docker-compose.yml`'s
`pgdata` volume is minor clutter (an unused `app` schema) for as long as the rollback path is
kept; deleting `docker-compose.subquery.yml`, the commented CI/deploy blocks, and `grpc-api/`
together is the natural cleanup once the migration is confirmed stable in production and the
rollback path is no longer wanted.
